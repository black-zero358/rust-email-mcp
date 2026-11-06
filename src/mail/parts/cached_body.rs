//! Full decoded inline leaves, before presentation or response truncation.
use super::*;
use crate::{
    cache::{ArtifactKey, ArtifactLayer, BlobLease, CacheError},
    html::{DisplayBody, DisplayContext, HtmlDisplay},
};
use std::io::{Read, Write};

pub(in crate::mail) const KEY: ArtifactKey = ArtifactKey::FullBody {
    mime_version: 1,
    html_version: 1,
};
pub(in crate::mail) const LAYER: ArtifactLayer = ArtifactLayer::FullBody {
    mime_version: 1,
    html_version: 1,
};
pub(in crate::mail) const LIMIT: usize = 32 * 1024 * 1024;
const MAGIC: &[u8; 4] = b"R11B";
const LEAF_BYTES: usize = 9;

#[derive(Default)]
pub(in crate::mail) struct Record {
    leaves: Vec<Leaf>,
}

struct Leaf {
    text: String,
    html: bool,
    reserve: usize,
}

impl Record {
    pub(in crate::mail) fn push(&mut self, mut text: String, html: bool, reserve: usize) {
        if text.capacity() > reserve {
            text.shrink_to_fit();
        }
        self.leaves.push(Leaf {
            text,
            html,
            reserve,
        });
    }

    pub(in crate::mail) fn bytes(&self) -> usize {
        self.leaves.iter().map(|leaf| leaf.text.len()).sum()
    }

    pub(in crate::mail) fn encode(&self, mut payload: impl Write) -> std::io::Result<Vec<u8>> {
        let mut metadata = Vec::with_capacity(MAGIC.len() + self.leaves.len() * LEAF_BYTES);
        metadata.extend_from_slice(MAGIC);
        for leaf in &self.leaves {
            metadata.push(u8::from(leaf.html));
            metadata.extend_from_slice(&(leaf.text.len() as u32).to_le_bytes());
            metadata.extend_from_slice(&(leaf.reserve as u32).to_le_bytes());
            payload.write_all(leaf.text.as_bytes())?;
        }
        Ok(metadata)
    }

    pub(in crate::mail) fn decode(lease: &mut BlobLease) -> std::result::Result<Self, CacheError> {
        let metadata = lease.body_metadata().ok_or(CacheError::Corrupt)?;
        let descriptors = metadata.strip_prefix(MAGIC).ok_or(CacheError::Corrupt)?;
        let (chunks, trailing) = descriptors.as_chunks::<LEAF_BYTES>();
        if !trailing.is_empty() || chunks.len() > mime::MAX_PARTS {
            return Err(CacheError::Corrupt);
        }
        let mut total = 0;
        let mut reserved = 0;
        let mut leaves = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            let html = match chunk[0] {
                0 => false,
                1 => true,
                _ => return Err(CacheError::Corrupt),
            };
            let bytes = u32::from_le_bytes(chunk[1..5].try_into().unwrap()) as usize;
            let reserve = u32::from_le_bytes(chunk[5..9].try_into().unwrap()) as usize;
            if bytes > reserve || reserve > LIMIT - reserved {
                return Err(CacheError::Corrupt);
            }
            reserved += reserve;
            total += bytes;
            leaves.push((bytes, html, reserve));
        }
        if total as u64 != lease.info().bytes {
            return Err(CacheError::Corrupt);
        }
        let mut record = Self::default();
        for (bytes, html, reserve) in leaves {
            let mut text = vec![0; bytes];
            lease.read_exact(&mut text)?;
            record.push(
                String::from_utf8(text).map_err(|_| CacheError::Corrupt)?,
                html,
                reserve,
            );
        }
        lease.revalidate()?;
        Ok(record)
    }

    pub(in crate::mail) fn display(&self, format: HtmlDisplay) -> Result<DisplayBody> {
        let reserved = self.leaves.iter().map(|leaf| leaf.reserve).sum::<usize>();
        let readable_extra =
            if format == HtmlDisplay::Readable && self.leaves.iter().any(|leaf| leaf.html) {
                crate::html::MAX_INPUT
            } else {
                0
            };
        let mut remaining = LIMIT
            .checked_sub(reserved)
            .ok_or_else(|| MailError::limit_kind(LimitKind::DecodedBodyBytes))?;
        let capacity = (reserved + self.leaves.len().saturating_sub(1)).min(LIMIT);
        let mut text = String::with_capacity(capacity + readable_extra);
        let mut context = DisplayContext::new(format);
        let mut work = crate::html::RenderWork::default();
        for leaf in &self.leaves {
            crate::html::project_body_leaf(
                &mut text,
                &leaf.text,
                leaf.html,
                leaf.reserve,
                &mut remaining,
                &mut context,
                &mut work,
            )?;
        }
        Ok(DisplayBody {
            text,
            metadata: context.into_metadata(),
        })
    }
}
