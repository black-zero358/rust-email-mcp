//! One attachment traversal contract for bounded BODYSTRUCTURE and raw MIME.
pub(in crate::mail) mod body;
mod body_tree;
pub(super) mod charset_decode;
pub(in crate::mail) mod fetch;
mod mapping;
pub(in crate::mail) mod preview;
mod raw;
pub(in crate::mail) mod reader;
pub(super) mod transfer_decode;

pub(super) use mapping::{ManifestPart, PartManifest};
pub(super) use raw::from_raw;
#[cfg(test)]
pub(super) use raw::raw_leaves;
pub(super) use reader::{ManifestSource, map_read_imap_error};

use super::*;
