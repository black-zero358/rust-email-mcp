//! One attachment traversal contract for bounded BODYSTRUCTURE and raw MIME.
mod mapping;
mod raw;
mod reader;

pub(super) use mapping::{ManifestPart, PartManifest};
pub(super) use raw::from_raw;
#[cfg(test)]
pub(super) use raw::raw_leaves;
pub(super) use reader::{ManifestSource, map_read_imap_error};

use super::*;
