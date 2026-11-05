//! Shared, codec-free selection of complete inline MIME representations.

pub(crate) trait Node: Copy {
    fn media_type(&self) -> &str;
    fn attachment(self) -> bool;
    fn encoding_supported(self) -> bool;
    fn nonempty(self) -> Option<bool>;
    fn children(self) -> usize;
    fn child(self, index: usize) -> Self;
    fn related_root(self) -> Option<Self>;
}

/// An error asks the caller to establish encoded emptiness for that entity.
/// Charset and transfer decoding never participate in an alternative probe.
pub(crate) fn select<N: Node>(node: N, out: &mut Vec<N>, html: bool) -> Result<bool, N> {
    if node.attachment() {
        return Ok(false);
    }
    if !node.encoding_supported() {
        return node.nonempty().ok_or(node);
    }
    if node.children() == 0 {
        match node.media_type() {
            "text/plain" => out.push(node),
            "text/html" if html => out.push(node),
            _ => return node.nonempty().ok_or(node),
        }
        return Ok(false);
    }
    match node.media_type() {
        "multipart/alternative" => {
            let mut unsupported = false;
            for mode in [false, true].into_iter().take(if html { 2 } else { 1 }) {
                for index in (0..node.children()).rev() {
                    let before = out.len();
                    let failed = select(node.child(index), out, mode)?;
                    if failed {
                        out.truncate(before);
                        unsupported = true;
                    } else if out.len() != before {
                        return Ok(false);
                    }
                }
            }
            Ok(unsupported)
        }
        "multipart/related" => match node.related_root() {
            Some(root) => select(root, out, html),
            None => node.nonempty().ok_or(node),
        },
        _ => {
            let mut unsupported = false;
            for index in 0..node.children() {
                unsupported |= select(node.child(index), out, html)?;
            }
            Ok(unsupported)
        }
    }
}

impl Node for &super::Part<'_> {
    fn media_type(&self) -> &str {
        &self.ctype.mimetype
    }
    fn attachment(self) -> bool {
        self.is_attachment()
    }
    fn encoding_supported(self) -> bool {
        self.transfer_encoding().is_ok()
    }
    fn nonempty(self) -> Option<bool> {
        Some(!self.body.is_empty())
    }
    fn children(self) -> usize {
        self.subparts.len()
    }
    fn child(self, index: usize) -> Self {
        &self.subparts[index]
    }
    fn related_root(self) -> Option<Self> {
        use mailparse::MailHeaderMap;
        match self.ctype.params.get("start") {
            Some(start) => self.subparts.iter().find(|part| {
                part.headers
                    .get_first_value("Content-ID")
                    .is_some_and(|id| id.trim() == start.trim())
            }),
            None => self.subparts.first(),
        }
    }
}
