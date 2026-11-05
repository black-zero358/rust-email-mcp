//! Versioned, bounded persistence for a committed conversation cursor.
use super::{Cursor, Prepared, Replay};
use crate::mail::conversation::{
    graph::{Failure, RefKey, SearchField},
    state::{CacheStamp, CacheStamps, State},
};
use sha2::{Digest, Sha256};
use std::{
    cmp::Ordering,
    io::{Read, Write},
    mem::size_of,
};

pub(in crate::mail::conversation) const CODEC_VERSION: u16 = 1;
pub(super) const CACHE_CODEC_VERSION: u16 = 2;
const MAGIC: &[u8; 8] = b"REMCPCUR";
const CHECKSUM_BYTES: usize = 32;
const MAX_ENCODED: usize = 8 * 1024 * 1024;
const BLOCK: usize = 8 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::mail::conversation) struct Invalid;

pub(in crate::mail::conversation) type Result<T> = std::result::Result<T, Invalid>;

pub(in crate::mail::conversation) struct Encoder<'a> {
    output: &'a mut dyn Write,
    hash: Sha256,
    written: usize,
    buffer: [u8; BLOCK],
    buffered: usize,
}

pub(in crate::mail::conversation) struct Decoder<'a> {
    input: &'a mut dyn Read,
    hash: Sha256,
    remaining: usize,
    unallocated: usize,
    buffer: [u8; BLOCK],
    position: usize,
    buffered: usize,
}

macro_rules! integer {
    ($method:ident, $type:ty) => {
        impl Encoder<'_> {
            pub(in crate::mail::conversation) fn $method(&mut self, value: $type) -> Result<()> {
                self.bytes(&value.to_be_bytes())
            }
        }
        impl Decoder<'_> {
            pub(in crate::mail::conversation) fn $method(&mut self) -> Result<$type> {
                Ok(<$type>::from_be_bytes(self.array()?))
            }
        }
    };
}

integer!(u8, u8);
integer!(u16, u16);
integer!(u32, u32);
integer!(u64, u64);

impl Encoder<'_> {
    fn flush(&mut self) -> Result<()> {
        self.output
            .write_all(&self.buffer[..self.buffered])
            .map_err(|_| Invalid)?;
        self.buffered = 0;
        Ok(())
    }

    pub(in crate::mail::conversation) fn bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.written = self
            .written
            .checked_add(bytes.len())
            .filter(|n| *n <= MAX_ENCODED - CHECKSUM_BYTES)
            .ok_or(Invalid)?;
        self.hash.update(bytes);
        let mut bytes = bytes;
        while !bytes.is_empty() {
            let count = bytes.len().min(BLOCK - self.buffered);
            self.buffer[self.buffered..self.buffered + count].copy_from_slice(&bytes[..count]);
            self.buffered += count;
            bytes = &bytes[count..];
            if self.buffered == BLOCK {
                self.flush()?;
            }
        }
        Ok(())
    }

    pub(in crate::mail::conversation) fn index(&mut self, value: usize) -> Result<()> {
        self.u32(value.try_into().map_err(|_| Invalid)?)
    }

    pub(in crate::mail::conversation) fn boolean(&mut self, value: bool) -> Result<()> {
        self.u8(u8::from(value))
    }

    pub(in crate::mail::conversation) fn option<T>(
        &mut self,
        value: Option<T>,
        encode: impl FnOnce(&mut Self, T) -> Result<()>,
    ) -> Result<()> {
        self.boolean(value.is_some())?;
        if let Some(value) = value {
            encode(self, value)?;
        }
        Ok(())
    }

    pub(in crate::mail::conversation) fn string(&mut self, value: &str) -> Result<()> {
        self.u16(value.len().try_into().map_err(|_| Invalid)?)?;
        self.bytes(value.as_bytes())
    }

    pub(in crate::mail::conversation) fn failure(
        &mut self,
        failure: Option<Failure>,
    ) -> Result<()> {
        self.u8(match failure {
            None => 0,
            Some(Failure::Backend) => 1,
            Some(Failure::Protocol) => 2,
            Some(Failure::Timeout) => 3,
            Some(Failure::Limit) => 4,
            Some(Failure::Missing) => 5,
        })
    }

    pub(in crate::mail::conversation) fn reference(&mut self, key: RefKey) -> Result<()> {
        self.u8(key.folder)?;
        self.u32(key.uid)
    }

    pub(in crate::mail::conversation) fn field(&mut self, field: SearchField) -> Result<()> {
        self.u8(match field {
            SearchField::MessageId => 0,
            SearchField::References => 1,
            SearchField::InReplyTo => 2,
        })
    }
}

impl Decoder<'_> {
    pub(in crate::mail::conversation) fn with_unique_indices<T>(
        &mut self,
        values: &[T],
        compare: impl Fn(&T, &T) -> Ordering,
        inspect: impl FnOnce(&[u32]) -> bool,
    ) -> Result<()> {
        let count: u32 = values.len().try_into().map_err(|_| Invalid)?;
        let mut indices = self.vector::<u32>(values.len(), 0)?;
        indices.extend(0..count);
        let compare =
            |left: &u32, right: &u32| compare(&values[*left as usize], &values[*right as usize]);
        indices.sort_unstable_by(compare);
        let valid = indices
            .windows(2)
            .all(|pair| compare(&pair[0], &pair[1]) != Ordering::Equal)
            && inspect(&indices);
        let bytes = indices.capacity() * size_of::<u32>();
        drop(indices);
        self.unallocated += bytes;
        valid.then_some(()).ok_or(Invalid)
    }

    fn read_exact(&mut self, mut bytes: &mut [u8]) -> Result<()> {
        while !bytes.is_empty() {
            if self.position == self.buffered {
                self.buffered = self.input.read(&mut self.buffer).map_err(|_| Invalid)?;
                self.position = 0;
                if self.buffered == 0 {
                    return Err(Invalid);
                }
            }
            let count = bytes.len().min(self.buffered - self.position);
            bytes[..count].copy_from_slice(&self.buffer[self.position..self.position + count]);
            self.position += count;
            bytes = &mut bytes[count..];
        }
        Ok(())
    }

    fn bytes(&mut self, bytes: &mut [u8]) -> Result<()> {
        self.remaining = self.remaining.checked_sub(bytes.len()).ok_or(Invalid)?;
        self.read_exact(bytes)?;
        self.hash.update(bytes);
        Ok(())
    }

    pub(in crate::mail::conversation) fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut bytes = [0; N];
        self.bytes(&mut bytes)?;
        Ok(bytes)
    }

    pub(in crate::mail::conversation) fn index(&mut self) -> Result<usize> {
        self.u32()?.try_into().map_err(|_| Invalid)
    }

    pub(in crate::mail::conversation) fn boolean(&mut self) -> Result<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Invalid),
        }
    }

    pub(in crate::mail::conversation) fn option<T>(
        &mut self,
        decode: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<Option<T>> {
        self.boolean()?.then(|| decode(self)).transpose()
    }

    pub(in crate::mail::conversation) fn charge(&mut self, bytes: usize) -> Result<()> {
        self.unallocated = self.unallocated.checked_sub(bytes).ok_or(Invalid)?;
        Ok(())
    }

    pub(in crate::mail::conversation) fn vector<T>(
        &mut self,
        count: usize,
        min_record_bytes: usize,
    ) -> Result<Vec<T>> {
        if count.checked_mul(min_record_bytes).ok_or(Invalid)? > self.remaining {
            return Err(Invalid);
        }
        self.charge(count.checked_mul(size_of::<T>()).ok_or(Invalid)?)?;
        let mut values = Vec::new();
        values.try_reserve_exact(count).map_err(|_| Invalid)?;
        self.charge(
            (values.capacity() - count)
                .checked_mul(size_of::<T>())
                .ok_or(Invalid)?,
        )?;
        Ok(values)
    }

    pub(in crate::mail::conversation) fn string<const N: usize>(
        &mut self,
        scratch: &mut [u8; N],
    ) -> Result<usize> {
        let len = self.u16()? as usize;
        if len == 0 || len > N || len > self.remaining {
            return Err(Invalid);
        }
        self.charge(len)?;
        self.bytes(&mut scratch[..len])?;
        Ok(len)
    }

    pub(in crate::mail::conversation) fn failure(&mut self) -> Result<Option<Failure>> {
        Ok(match self.u8()? {
            0 => None,
            1 => Some(Failure::Backend),
            2 => Some(Failure::Protocol),
            3 => Some(Failure::Timeout),
            4 => Some(Failure::Limit),
            5 => Some(Failure::Missing),
            _ => return Err(Invalid),
        })
    }

    pub(in crate::mail::conversation) fn reference(&mut self) -> Result<RefKey> {
        Ok(RefKey {
            folder: self.u8()?,
            uid: self.u32()?,
        })
    }

    pub(in crate::mail::conversation) fn field(&mut self) -> Result<SearchField> {
        SearchField::ALL
            .get(self.u8()? as usize)
            .copied()
            .ok_or(Invalid)
    }
}

pub(super) fn encode(
    output: &mut dyn Write,
    state: &State,
    prepared: &Prepared,
    record_replay: bool,
) -> Result<usize> {
    let mut encoder = Encoder {
        output,
        hash: Sha256::new(),
        written: 0,
        buffer: [0; BLOCK],
        buffered: 0,
    };
    encoder.bytes(MAGIC)?;
    encoder.u16(codec_version(state))?;
    encoder.u16(state.version())?;
    state.encode(&mut encoder, &prepared.delivery)?;
    if let Some(stamps) = state.cache_stamps() {
        for stamp in stamps {
            encoder.option(*stamp, |e, stamp| {
                e.u64(stamp.checked_at_ms.try_into().map_err(|_| Invalid)?)?;
                e.u64(stamp.index_revision)?;
                e.u64(stamp.dirty_revision)?;
                e.u64(stamp.restore_epoch)?;
                e.u64(stamp.complete_headers)?;
                e.u64(stamp.indexed_threads)?;
                e.u64(stamp.known_messages)
            })?;
        }
    }
    encoder.boolean(record_replay)?;
    if record_replay {
        encoder.bytes(prepared.input.0.as_bytes())?;
        encoder.option(prepared.next, |e, token| e.bytes(token.0.as_bytes()))?;
        prepared.delivery.encode(&mut encoder)?;
        prepared.coverage.encode(&mut encoder)?;
        for coverage in &prepared.folders {
            encoder.option(*coverage, |e, coverage| coverage.encode(e))?;
        }
        prepared.errors.encode(&mut encoder)?;
        encoder.boolean(prepared.capacity_reached)?;
        encoder.boolean(prepared.wire_omitted)?;
    }
    encoder.flush()?;
    encoder
        .output
        .write_all(&encoder.hash.finalize())
        .map_err(|_| Invalid)?;
    Ok(encoder.written + CHECKSUM_BYTES)
}

pub(super) fn decode(
    input: &mut dyn Read,
    encoded_len: usize,
    owned_limit: usize,
    namespace: [u8; 32],
) -> Result<(State, Option<Replay>)> {
    if !(CHECKSUM_BYTES..=MAX_ENCODED).contains(&encoded_len) {
        return Err(Invalid);
    }
    let mut decoder = Decoder {
        input,
        hash: Sha256::new(),
        remaining: encoded_len - CHECKSUM_BYTES,
        unallocated: owned_limit,
        buffer: [0; BLOCK],
        position: 0,
        buffered: 0,
    };
    if &decoder.array::<8>()? != MAGIC {
        return Err(Invalid);
    }
    let version = decoder.u16()?;
    if !supported(version) || decoder.u16()? != super::STATE_VERSION {
        return Err(Invalid);
    }
    let mut state = State::decode(&mut decoder, namespace)?;
    if version == CACHE_CODEC_VERSION {
        let mut stamps = [None; super::MAX_FOLDERS];
        for stamp in &mut stamps {
            *stamp = decoder.option(|d| {
                Ok(CacheStamp {
                    checked_at_ms: d.u64()?.try_into().map_err(|_| Invalid)?,
                    index_revision: d.u64()?,
                    dirty_revision: d.u64()?,
                    restore_epoch: d.u64()?,
                    complete_headers: d.u64()?,
                    indexed_threads: d.u64()?,
                    known_messages: d.u64()?,
                })
            })?;
        }
        decoder.charge(size_of::<CacheStamps>())?;
        state
            .set_cache_stamps(stamps, owned_limit)
            .map_err(|_| Invalid)?;
    }
    if state.owned_bytes() > owned_limit {
        return Err(Invalid);
    }
    let replay = decoder.option(|d| {
        let input = Cursor(uuid::Uuid::from_bytes(d.array()?));
        let next = d.option(|d| Ok(Cursor(uuid::Uuid::from_bytes(d.array()?))))?;
        let delivery = super::Delivery::decode(d, &state)?;
        let coverage = super::Coverage::decode(d)?;
        let mut folders = [None; super::MAX_FOLDERS];
        for (index, folder) in folders.iter_mut().enumerate() {
            *folder = d.option(super::FolderCoverage::decode)?;
            if *folder != state.folder_coverage(index as u8) {
                return Err(Invalid);
            }
        }
        let errors = super::ErrorSnapshot::decode(d, &state)?;
        if errors != state.error_snapshot()
            || coverage != state.coverage()
            || next.is_none() != (coverage.graph_complete && !coverage.delivery_pending)
            || next == Some(input)
            || input.cached() != state.cache_stamps().is_some()
            || next.is_some_and(|next| next.cached() != input.cached())
        {
            return Err(Invalid);
        }
        Ok(Replay {
            input,
            next,
            delivery,
            coverage,
            folders,
            errors,
            capacity_reached: d.boolean()?,
            wire_omitted: d.boolean()?,
        })
    })?;
    if decoder.remaining != 0 {
        return Err(Invalid);
    }
    let mut checksum = [0; CHECKSUM_BYTES];
    decoder.read_exact(&mut checksum)?;
    if checksum != <[u8; CHECKSUM_BYTES]>::from(decoder.hash.finalize())
        || decoder.position != decoder.buffered
        || decoder.input.read(&mut [0]).map_err(|_| Invalid)? != 0
    {
        return Err(Invalid);
    }
    Ok((state, replay))
}

pub(super) fn codec_version(state: &State) -> u16 {
    if state.cache_stamps().is_some() {
        CACHE_CODEC_VERSION
    } else {
        CODEC_VERSION
    }
}

pub(super) fn supported(version: u16) -> bool {
    matches!(version, CODEC_VERSION | CACHE_CODEC_VERSION)
}

#[cfg(test)]
#[path = "tests/persist.rs"]
mod tests;
