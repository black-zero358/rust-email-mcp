//! Resumable attachment transfer decoding; charset conversion belongs to body reads.
use std::mem;

pub(super) const OUTPUT_CHUNK: usize = 64 * 1024;
const MAX_DECODED_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Encoding {
    Identity,
    Base64,
    QuotedPrintable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DecodeError {
    InvalidBase64,
    DecodedLimit { limit: usize },
    PendingAllocation,
    InputCountOverflow,
    InvalidOutputBuffer,
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StepStatus {
    NeedInput,
    NeedOutput,
    Finished,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Step {
    pub consumed: usize,
    pub produced: usize,
    pub status: StepStatus,
}

#[derive(Clone, Copy, Debug, Default)]
struct Stats {
    encoded_bytes: usize,
    decoded_bytes: usize,
    produced_bytes: usize,
    peak_pending_capacity: usize,
}

#[derive(Default)]
struct Base64 {
    group: [u8; 4],
    used: usize,
}

fn six(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

impl Base64 {
    fn byte(&mut self, byte: u8, run: &mut Run) -> Result<(), DecodeError> {
        if byte.is_ascii_whitespace() {
            return Ok(());
        }
        if byte != b'=' && six(byte).is_none() {
            return Err(DecodeError::InvalidBase64);
        }
        self.group[self.used] = byte;
        self.used += 1;
        if self.used == 4 {
            let a = six(self.group[0]).ok_or(DecodeError::InvalidBase64)?;
            let b = six(self.group[1]).ok_or(DecodeError::InvalidBase64)?;
            let (c, d, count) = match (self.group[2], self.group[3]) {
                (b'=', b'=') => (0, 0, 1),
                (c, b'=') => (six(c).ok_or(DecodeError::InvalidBase64)?, 0, 2),
                (c, d) => (
                    six(c).ok_or(DecodeError::InvalidBase64)?,
                    six(d).ok_or(DecodeError::InvalidBase64)?,
                    3,
                ),
            };
            run.set(&[(a << 2) | (b >> 4), (b << 4) | (c >> 2), (c << 6) | d][..count]);
            self.used = 0;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Default)]
enum Escape {
    #[default]
    Normal,
    Equals,
    Upper(u8),
}

#[derive(Default)]
struct QuotedPrintable {
    escape: Escape,
    pending: Vec<u8>,
    cursor: usize,
    overflow: bool,
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

impl QuotedPrintable {
    fn raw(&mut self, byte: u8, run: &mut Run) {
        match mem::replace(&mut self.escape, Escape::Normal) {
            Escape::Normal => {
                if byte == b'=' {
                    self.escape = Escape::Equals;
                } else {
                    run.set(&[byte]);
                }
            }
            Escape::Equals => self.escape = Escape::Upper(byte),
            Escape::Upper(upper) => match (hex(upper), hex(byte)) {
                (Some(a), Some(b)) => run.set(&[(a << 4) | b]),
                _ => run.set(&[b'=', upper, byte]),
            },
        }
    }

    fn whitespace(
        &mut self,
        byte: u8,
        remaining: usize,
        stats: &mut Stats,
    ) -> Result<(), DecodeError> {
        if self.pending.len() >= remaining {
            self.overflow = true;
            return Ok(());
        }
        if self.pending.len() == self.pending.capacity() {
            let next = self
                .pending
                .capacity()
                .checked_mul(2)
                .map(|n| n.max(1))
                .ok_or(DecodeError::PendingAllocation)?
                .min(remaining);
            self.pending
                .try_reserve_exact(next - self.pending.len())
                .map_err(|_| DecodeError::PendingAllocation)?;
            if self.pending.capacity() > remaining {
                self.pending = Vec::new();
                return Err(DecodeError::PendingAllocation);
            }
            stats.peak_pending_capacity = stats.peak_pending_capacity.max(self.pending.capacity());
        }
        self.pending.push(byte);
        Ok(())
    }

    fn end_line(&mut self, newline: bool, run: &mut Run) {
        self.pending.clear();
        self.cursor = 0;
        self.overflow = false;
        let soft = matches!(self.escape, Escape::Equals);
        let mut bytes = [0; 4];
        let mut count = 0;
        if let Escape::Upper(upper) = self.escape {
            bytes[..2].copy_from_slice(&[b'=', upper]);
            count = 2;
        }
        self.escape = Escape::Normal;
        if newline && !soft {
            bytes[count..count + 2].copy_from_slice(b"\r\n");
            count += 2;
        }
        run.set(&bytes[..count]);
    }

    // Returns false while an interior whitespace run is being drained. The
    // triggering nonwhitespace input byte remains unconsumed across steps.
    fn byte(
        &mut self,
        byte: u8,
        run: &mut Run,
        limit: usize,
        stats: &mut Stats,
    ) -> Result<bool, DecodeError> {
        match byte {
            b'\n' => self.end_line(true, run),
            b' ' | b'\t' | b'\r' => {
                self.whitespace(byte, limit - stats.decoded_bytes, stats)?;
            }
            b'!'..=b'~' => {
                if self.overflow {
                    return Err(DecodeError::DecodedLimit { limit });
                }
                if self.cursor < self.pending.len() {
                    let pending = self.pending[self.cursor];
                    self.cursor += 1;
                    self.raw(pending, run);
                    return Ok(false);
                }
                self.pending.clear();
                self.cursor = 0;
                self.raw(byte, run);
            }
            _ => {} // Match the existing Robust codec's ignored input bytes.
        }
        Ok(true)
    }
}

#[derive(Default)]
struct Run {
    bytes: [u8; 4],
    len: usize,
    cursor: usize,
}
impl Run {
    fn set(&mut self, bytes: &[u8]) {
        self.bytes[..bytes.len()].copy_from_slice(bytes);
        self.len = bytes.len();
        self.cursor = 0;
    }
}

enum State {
    Identity,
    Base64(Base64),
    QuotedPrintable(QuotedPrintable),
}

pub(super) struct Decoder {
    state: State,
    run: Run,
    limit: usize,
    stats: Stats,
    finishing: bool,
    closed: bool,
}

impl Decoder {
    pub fn new(encoding: Encoding, decoded_limit: usize) -> Self {
        Self {
            state: match encoding {
                Encoding::Identity => State::Identity,
                Encoding::Base64 => State::Base64(Base64::default()),
                Encoding::QuotedPrintable => State::QuotedPrintable(QuotedPrintable::default()),
            },
            run: Run::default(),
            limit: decoded_limit.min(MAX_DECODED_BYTES),
            stats: Stats::default(),
            finishing: false,
            closed: false,
        }
    }

    /// `eof` marks the final input slice; retain its unconsumed suffix on
    /// NeedOutput and pass eof=true again. Output must contain 1..=64 KiB.
    /// Generated bytes are charged before becoming visible in output. Callers
    /// discard provisional output and this decoder after any I/O failure.
    pub fn step(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        eof: bool,
    ) -> Result<Step, DecodeError> {
        if self.closed {
            return Err(DecodeError::Closed);
        }
        let result = self.step_inner(input, output, eof);
        if result.is_err() {
            self.closed = true;
        }
        result
    }

    fn step_inner(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        eof: bool,
    ) -> Result<Step, DecodeError> {
        if self.finishing && !input.is_empty() {
            return Err(DecodeError::Closed);
        }
        if output.is_empty() || output.len() > OUTPUT_CHUNK {
            return Err(DecodeError::InvalidOutputBuffer);
        }
        let mut consumed = 0;
        let mut produced = 0;
        loop {
            if self.run.cursor < self.run.len {
                let count = (self.run.len - self.run.cursor).min(output.len() - produced);
                output[produced..produced + count]
                    .copy_from_slice(&self.run.bytes[self.run.cursor..self.run.cursor + count]);
                self.run.cursor += count;
                produced += count;
                self.stats.produced_bytes += count; // Bounded by the already charged decoded cap.
                if self.run.cursor < self.run.len || produced == output.len() {
                    return Ok(Step {
                        consumed,
                        produced,
                        status: StepStatus::NeedOutput,
                    });
                }
            }
            self.run.set(&[]);
            if self.finishing {
                self.closed = true;
                return Ok(Step {
                    consumed,
                    produced,
                    status: StepStatus::Finished,
                });
            }
            if consumed == input.len() {
                if !eof {
                    return Ok(Step {
                        consumed,
                        produced,
                        status: StepStatus::NeedInput,
                    });
                }
                match &mut self.state {
                    State::Identity => {}
                    State::Base64(state) => {
                        if state.used != 0 {
                            return Err(DecodeError::InvalidBase64);
                        }
                    }
                    State::QuotedPrintable(state) => state.end_line(false, &mut self.run),
                }
                self.finishing = true;
            } else {
                self.stats
                    .encoded_bytes
                    .checked_add(1)
                    .ok_or(DecodeError::InputCountOverflow)?;
                let byte = input[consumed];
                let used = match &mut self.state {
                    State::Identity => {
                        self.run.set(&[byte]);
                        true
                    }
                    State::Base64(state) => {
                        state.byte(byte, &mut self.run)?;
                        true
                    }
                    State::QuotedPrintable(state) => {
                        state.byte(byte, &mut self.run, self.limit, &mut self.stats)?
                    }
                };
                if used {
                    consumed += 1;
                    self.stats.encoded_bytes += 1;
                }
            }
            self.stats.decoded_bytes = self
                .stats
                .decoded_bytes
                .checked_add(self.run.len)
                .filter(|count| *count <= self.limit)
                .ok_or(DecodeError::DecodedLimit { limit: self.limit })?;
        }
    }
}

#[cfg(test)]
#[path = "transfer_decode/tests.rs"]
mod tests;
