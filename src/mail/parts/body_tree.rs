//! Header-confirmed body topology; leaf metadata lives in the original DFS table.
use super::mapping::{PartManifest, Section};

#[derive(Debug, Eq, PartialEq, Default)]
pub(super) struct BodyTree {
    pub nodes: Vec<BodyNode>,
    pub root_headers: Vec<u8>,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct BodyNode {
    pub parent: Option<usize>,
    pub children: Vec<usize>,
    pub kind: BodyKind,
    pub nonempty: Option<bool>,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum BodyKind {
    Leaf(usize),
    Container {
        section: Section,
        media_type: String,
        encoding: String,
        content_id: Option<String>,
        related_start: Option<String>,
    },
}

#[derive(Clone, Copy)]
pub(super) struct Node<'a> {
    pub manifest: &'a PartManifest,
    pub index: usize,
}
impl Node<'_> {
    fn node(&self) -> &BodyNode {
        &self.manifest.body.nodes[self.index]
    }
    fn cid(&self) -> Option<&str> {
        match &self.node().kind {
            BodyKind::Leaf(index) => self.manifest.parts[*index].content_id.as_deref(),
            BodyKind::Container { content_id, .. } => content_id.as_deref(),
        }
    }
}
impl crate::mime::selection::Node for Node<'_> {
    fn media_type(&self) -> &str {
        match &self.node().kind {
            BodyKind::Leaf(index) => &self.manifest.parts[*index].content_type,
            BodyKind::Container { media_type, .. } => media_type,
        }
    }
    fn attachment(self) -> bool {
        matches!(&self.node().kind, BodyKind::Leaf(index) if self.manifest.parts[*index].is_attachment)
    }
    fn encoding_supported(self) -> bool {
        match &self.node().kind {
            BodyKind::Leaf(index) => self.manifest.parts[*index].downloadable,
            BodyKind::Container { encoding, .. } => matches!(
                encoding.as_str(),
                "7bit" | "8bit" | "binary" | "base64" | "quoted-printable"
            ),
        }
    }
    fn charset_supported(self) -> bool {
        match &self.node().kind {
            BodyKind::Leaf(index) => {
                crate::mime::selection::charset_supported(&self.manifest.parts[*index].charset)
            }
            BodyKind::Container { .. } => true,
        }
    }
    fn nonempty(self) -> Option<bool> {
        match &self.node().kind {
            BodyKind::Leaf(index) => self.manifest.parts[*index]
                .encoded_size
                .map(|n| n != 0)
                .or(self.node().nonempty),
            BodyKind::Container { .. } => self.node().nonempty,
        }
    }
    fn children(self) -> usize {
        self.node().children.len()
    }
    fn child(self, index: usize) -> Self {
        Self {
            manifest: self.manifest,
            index: self.node().children[index],
        }
    }
    fn related_root(self) -> Option<Self> {
        let BodyKind::Container { related_start, .. } = &self.node().kind else {
            return None;
        };
        match related_start {
            Some(start) => self
                .node()
                .children
                .iter()
                .copied()
                .map(|index| Self {
                    manifest: self.manifest,
                    index,
                })
                .find(|node| node.cid().is_some_and(|cid| cid.trim() == start.trim())),
            None => self.node().children.first().map(|index| Self {
                manifest: self.manifest,
                index: *index,
            }),
        }
    }
}
