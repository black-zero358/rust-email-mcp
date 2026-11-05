//! Bounded BODYSTRUCTURE mapping confirmed by the original MIME header semantics.
//! The protocol parser must enforce its own allocation budget before this stage.
use super::body_tree::{BodyKind, BodyNode, BodyTree};
use imap_proto::{
    BodyContentCommon, BodyContentSinglePart, BodyExtension, BodyParams, BodyStructure,
    ContentEncoding,
};
use mailparse::MailHeaderMap;
use std::mem::size_of;

const MAX_DEPTH: usize = 32;
const MAX_NODES: usize = 1024;
const MAX_STRING: usize = 128 * 1024;
const MAX_NODE_PARAMS: usize = 256;
const MAX_PARAMS: usize = 8192;
const MAX_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Section(pub(super) Vec<u32>);
impl Section {
    pub fn numbers(&self) -> &[u32] {
        &self.0
    }
    pub fn body_selector(&self) -> String {
        self.0
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(".")
    }
    pub(super) fn child(&self, position: usize) -> Result<Self, MapError> {
        let number = u32::try_from(position.checked_add(1).ok_or(MapError::StructureLimit)?)
            .map_err(|_| MapError::StructureLimit)?;
        let mut path = self.0.clone();
        path.push(number);
        Ok(Self(path))
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeaderRequest {
    pub section: Section,
    pub root_headers: bool,
    pub reason: &'static str,
}
pub struct HeaderEvidence {
    pub section: Section,
    pub bytes: Vec<u8>,
}
#[derive(Debug, Eq, PartialEq)]
pub enum ManifestOutcome {
    Ready(PartManifest),
    NeedsHeaders(Vec<HeaderRequest>),
    RawFallback { reason: &'static str },
}
#[derive(Debug, Eq, PartialEq)]
pub struct PartManifest {
    pub parts: Vec<ManifestPart>,
    pub(super) body: BodyTree,
    #[cfg(test)]
    pub(super) accounted_bytes: usize,
}

impl PartManifest {
    pub(in crate::mail) fn root_headers(&self) -> &[u8] {
        &self.body.root_headers
    }
}
#[derive(Debug, Eq, PartialEq)]
pub struct ManifestPart {
    pub section: Section,
    /// Original DFS position, including non-attachment leaves.
    pub attachment_index: usize,
    pub attachment_ordinal: Option<usize>,
    pub is_attachment: bool,
    pub content_type: String,
    pub(super) charset: String,
    pub transfer_encoding: String,
    pub encoded_size: Option<u32>,
    pub disposition: Option<String>,
    pub filename: Option<String>,
    pub content_id: Option<String>,
    pub downloadable: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MapError {
    StructureLimit,
    HeaderLimit,
}
#[derive(Default)]
struct Budget {
    nodes: usize,
    params: usize,
    bytes: usize,
}
impl Budget {
    fn charge(&mut self, n: usize) -> Result<(), MapError> {
        self.bytes = self.bytes.checked_add(n).ok_or(MapError::StructureLimit)?;
        if self.bytes > MAX_BYTES {
            return Err(MapError::StructureLimit);
        }
        Ok(())
    }
    fn grow<T>(&mut self, values: &mut Vec<T>, additional: usize) -> Result<(), MapError> {
        let needed = values
            .len()
            .checked_add(additional)
            .ok_or(MapError::StructureLimit)?;
        if needed <= values.capacity() {
            return Ok(());
        }
        let old = values.capacity();
        let requested = needed
            .checked_sub(old)
            .and_then(|n| n.checked_mul(size_of::<T>()))
            .ok_or(MapError::StructureLimit)?;
        self.charge(requested)?;
        values
            .try_reserve_exact(additional)
            .map_err(|_| MapError::StructureLimit)?;
        let actual = values
            .capacity()
            .checked_sub(needed)
            .and_then(|n| n.checked_mul(size_of::<T>()))
            .ok_or(MapError::StructureLimit)?;
        self.charge(actual)
    }
    fn owned(&mut self, value: &String) -> Result<(), MapError> {
        self.charge(value.capacity())
    }
    fn owned_optional(&mut self, value: &Option<String>) -> Result<(), MapError> {
        if let Some(value) = value {
            self.owned(value)?;
        }
        Ok(())
    }
    fn section(&mut self, section: &Section) -> Result<(), MapError> {
        self.charge(
            section
                .0
                .capacity()
                .checked_mul(size_of::<u32>())
                .ok_or(MapError::StructureLimit)?,
        )
    }
    fn string(&mut self, value: &str) -> Result<(), MapError> {
        if value.len() > MAX_STRING {
            return Err(MapError::StructureLimit);
        }
        self.charge(value.len())
    }
    fn optional(&mut self, value: Option<&str>) -> Result<(), MapError> {
        if let Some(value) = value {
            self.string(value)?;
        }
        Ok(())
    }
    fn parameters(
        &mut self,
        params: &BodyParams<'_>,
        node_pairs: &mut usize,
    ) -> Result<(), MapError> {
        if let Some(params) = params {
            *node_pairs = node_pairs
                .checked_add(params.len())
                .ok_or(MapError::StructureLimit)?;
            self.params = self
                .params
                .checked_add(params.len())
                .ok_or(MapError::StructureLimit)?;
            if *node_pairs > MAX_NODE_PARAMS || self.params > MAX_PARAMS {
                return Err(MapError::StructureLimit);
            }
            self.charge(
                params
                    .len()
                    .checked_mul(size_of::<(String, String)>())
                    .ok_or(MapError::StructureLimit)?,
            )?;
            for (key, value) in params {
                self.string(key)?;
                self.string(value)?;
            }
        }
        Ok(())
    }
    fn extension(&mut self, ext: &BodyExtension<'_>, depth: usize) -> Result<(), MapError> {
        if depth > MAX_DEPTH {
            return Err(MapError::StructureLimit);
        }
        self.charge(size_of::<BodyExtension<'_>>())?;
        match ext {
            BodyExtension::Str(value) => self.optional(value.as_deref()),
            BodyExtension::Num(_) => Ok(()),
            BodyExtension::List(values) => {
                for value in values {
                    self.extension(value, depth + 1)?;
                }
                Ok(())
            }
        }
    }
    fn tree(&mut self, node: &BodyStructure<'_>, depth: usize) -> Result<(), MapError> {
        if depth > MAX_DEPTH || self.nodes >= MAX_NODES {
            return Err(MapError::StructureLimit);
        }
        self.nodes += 1;
        self.charge(size_of::<ManifestPart>())?;
        let (common, single, extension) = fields(node);
        self.string(&common.ty.ty)?;
        self.string(&common.ty.subtype)?;
        let mut pairs = 0;
        self.parameters(&common.ty.params, &mut pairs)?;
        if let Some(disposition) = &common.disposition {
            self.string(&disposition.ty)?;
            self.parameters(&disposition.params, &mut pairs)?;
        }
        self.optional(common.location.as_deref())?;
        if let Some(languages) = &common.language {
            self.charge(
                languages
                    .len()
                    .checked_mul(size_of::<String>())
                    .ok_or(MapError::StructureLimit)?,
            )?;
            for value in languages {
                self.string(value)?;
            }
        }
        if let Some(single) = single {
            self.optional(single.id.as_deref())?;
            self.optional(single.md5.as_deref())?;
            self.optional(single.description.as_deref())?;
            if let ContentEncoding::Other(value) = &single.transfer_encoding {
                self.string(value)?;
            }
        }
        if let Some(extension) = extension {
            self.extension(extension, depth)?;
        }
        match node {
            BodyStructure::Multipart { bodies, .. } => {
                for child in bodies {
                    self.tree(child, depth + 1)?;
                }
            }
            BodyStructure::Message { body, envelope, .. } => {
                for value in [
                    &envelope.date,
                    &envelope.subject,
                    &envelope.in_reply_to,
                    &envelope.message_id,
                ]
                .into_iter()
                .flatten()
                {
                    if value.len() > MAX_STRING {
                        return Err(MapError::StructureLimit);
                    }
                    self.charge(value.len())?;
                }
                for addresses in [
                    &envelope.from,
                    &envelope.sender,
                    &envelope.reply_to,
                    &envelope.to,
                    &envelope.cc,
                    &envelope.bcc,
                ]
                .into_iter()
                .flatten()
                {
                    self.charge(
                        addresses
                            .len()
                            .checked_mul(size_of::<imap_proto::Address<'_>>())
                            .ok_or(MapError::StructureLimit)?,
                    )?;
                    for address in addresses {
                        for value in [&address.name, &address.adl, &address.mailbox, &address.host]
                            .into_iter()
                            .flatten()
                        {
                            if value.len() > MAX_STRING {
                                return Err(MapError::StructureLimit);
                            }
                            self.charge(value.len())?;
                        }
                    }
                }
                self.tree(body, depth + 1)?;
            }
            _ => {}
        }
        Ok(())
    }
}
fn fields<'a, 'b>(
    node: &'a BodyStructure<'b>,
) -> (
    &'a BodyContentCommon<'b>,
    Option<&'a BodyContentSinglePart<'b>>,
    Option<&'a BodyExtension<'b>>,
) {
    match node {
        BodyStructure::Basic {
            common,
            other,
            extension,
        }
        | BodyStructure::Text {
            common,
            other,
            extension,
            ..
        }
        | BodyStructure::Message {
            common,
            other,
            extension,
            ..
        } => (common, Some(other), extension.as_ref()),
        BodyStructure::Multipart {
            common, extension, ..
        } => (common, None, extension.as_ref()),
    }
}
fn encoding(single: Option<&BodyContentSinglePart<'_>>) -> String {
    match single.map(|v| &v.transfer_encoding) {
        Some(ContentEncoding::SevenBit) | None => "7bit".into(),
        Some(ContentEncoding::EightBit) => "8bit".into(),
        Some(ContentEncoding::Binary) => "binary".into(),
        Some(ContentEncoding::Base64) => "base64".into(),
        Some(ContentEncoding::QuotedPrintable) => "quoted-printable".into(),
        Some(ContentEncoding::Other(value)) => value.trim().to_ascii_lowercase(),
    }
}
fn supported(encoding: &str) -> bool {
    matches!(
        encoding,
        "7bit" | "8bit" | "binary" | "base64" | "quoted-printable"
    )
}
fn has_name(params: &BodyParams<'_>, base: &str) -> bool {
    params.as_ref().is_some_and(|values| {
        values.iter().any(|(key, _)| {
            let key = key.to_ascii_lowercase();
            key == base
                || key
                    .strip_prefix(base)
                    .is_some_and(|tail| tail.starts_with('*'))
        })
    })
}
fn param<'a>(params: &'a BodyParams<'_>, name: &str) -> Option<&'a str> {
    params
        .as_ref()?
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_ref())
}
fn ambiguous(params: &BodyParams<'_>) -> bool {
    params.as_ref().is_some_and(|values| {
        values.iter().enumerate().any(|(i, (key, value))| {
            key.contains('*')
                || !key.is_ascii()
                || value.contains("=?")
                || !value.is_ascii()
                || key.bytes().any(|b| b.is_ascii_control() || b == b' ')
                || value.bytes().any(|b| b.is_ascii_control())
                || values[..i]
                    .iter()
                    .any(|(previous, _)| key.eq_ignore_ascii_case(previous))
        })
    })
}
struct Metadata {
    content_type: String,
    encoding: String,
    disposition: Option<String>,
    filename: Option<String>,
    cid: Option<String>,
    attachment: bool,
    charset: String,
    start: Option<String>,
}
fn ast_metadata(
    common: &BodyContentCommon<'_>,
    single: Option<&BodyContentSinglePart<'_>>,
) -> Metadata {
    let disposition = common
        .disposition
        .as_ref()
        .map(|value| value.ty.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty());
    let attachment = disposition
        .as_deref()
        .is_some_and(|v| !v.is_empty() && v != "inline")
        || common
            .disposition
            .as_ref()
            .is_some_and(|v| has_name(&v.params, "filename"))
        || has_name(&common.ty.params, "name");
    let filename = common
        .disposition
        .as_ref()
        .and_then(|v| param(&v.params, "filename"))
        .or_else(|| param(&common.ty.params, "name"))
        .map(str::to_owned);
    Metadata {
        content_type: format!(
            "{}/{}",
            common.ty.ty.to_ascii_lowercase(),
            common.ty.subtype.to_ascii_lowercase()
        ),
        encoding: encoding(single),
        disposition,
        filename,
        cid: single.and_then(|v| v.id.as_deref()).map(str::to_owned),
        attachment,
        charset: "us-ascii".into(),
        start: None,
    }
}

/// Complete MIME header blocks are caller-supplied evidence, never a user section.
/// On NeedsHeaders, no indices are exposed. A caller repeats with bounded evidence.
pub fn map(
    root: &BodyStructure<'_>,
    evidence: &[HeaderEvidence],
) -> Result<ManifestOutcome, MapError> {
    let mut budget = Budget::default();
    budget.tree(root, 1)?;
    let mut header_bytes = 0usize;
    let mut header_lines = 0;
    for (i, item) in evidence.iter().enumerate() {
        if item.bytes.len() > MAX_STRING {
            return Err(MapError::HeaderLimit);
        }
        header_bytes = header_bytes
            .checked_add(item.bytes.len())
            .ok_or(MapError::HeaderLimit)?;
        if header_bytes > MAX_BYTES || evidence.len() > MAX_NODES {
            return Err(MapError::HeaderLimit);
        }
        header_lines += item.bytes.split_inclusive(|byte| *byte == b'\n').count();
        if header_lines > crate::mime::MAX_HEADER_LINES {
            return Err(MapError::HeaderLimit);
        }
        if item.section.0.len() > MAX_DEPTH + 1 || item.section.0.contains(&0) {
            return Ok(ManifestOutcome::RawFallback {
                reason: "invalid_header_section",
            });
        }
        if evidence[..i]
            .iter()
            .any(|previous| previous.section == item.section)
        {
            return Ok(ManifestOutcome::RawFallback {
                reason: "duplicate_header_evidence",
            });
        }
    }
    let root_section = Section(if matches!(root, BodyStructure::Multipart { .. }) {
        vec![]
    } else {
        vec![1]
    });
    let mut builder = Builder {
        evidence,
        parts: vec![],
        needed: vec![],
        fallback: None,
        attachments: 0,
        body: BodyTree::default(),
        budget: Budget::default(),
    };
    builder.visit(root, root_section, true, false, None)?;
    if let Some(reason) = builder.fallback {
        return Ok(ManifestOutcome::RawFallback { reason });
    }
    if !builder.needed.is_empty() {
        return Ok(ManifestOutcome::NeedsHeaders(builder.needed));
    }
    Ok(ManifestOutcome::Ready(PartManifest {
        parts: builder.parts,
        body: builder.body,
        #[cfg(test)]
        accounted_bytes: builder.budget.bytes,
    }))
}
struct Builder<'a> {
    evidence: &'a [HeaderEvidence],
    parts: Vec<ManifestPart>,
    needed: Vec<HeaderRequest>,
    fallback: Option<&'static str>,
    attachments: usize,
    body: BodyTree,
    budget: Budget,
}
impl Builder<'_> {
    fn visit(
        &mut self,
        node: &BodyStructure<'_>,
        section: Section,
        root: bool,
        parent_digest: bool,
        parent: Option<usize>,
    ) -> Result<(), MapError> {
        if self.fallback.is_some() {
            return Ok(());
        }
        let (common, single, _) = fields(node);
        let mut meta = ast_metadata(common, single);
        let has_boundary;
        let ambiguous = common.disposition.is_none()
            || ambiguous(&common.ty.params)
            || common
                .disposition
                .as_ref()
                .is_some_and(|v| ambiguous(&v.params))
            || single.is_none();
        if let Some(headers) = self.evidence.iter().find(|item| item.section == section) {
            let raw = &headers.bytes;
            let mut end = 0;
            let mut complete = false;
            for line in raw.split_inclusive(|b| *b == b'\n') {
                end += line.len();
                if line == b"\r\n" || line == b"\n" {
                    complete = end == raw.len();
                    break;
                }
            }
            if !complete {
                self.fallback = Some("incomplete_mime_headers");
                return Ok(());
            }
            let Ok(mut part) = crate::mime::parse(raw) else {
                self.fallback = Some("invalid_mime_headers");
                return Ok(());
            };
            // RFC 2046 digest defaults apply only to immediate children. An
            // explicitly present (even empty) Content-Type keeps raw-parser semantics.
            if parent_digest && part.headers.get_first_value("Content-Type").is_none() {
                part.ctype = mailparse::parse_content_type("message/rfc822");
            }
            for name in [
                "Content-Type",
                "Content-Disposition",
                "Content-Transfer-Encoding",
                "Content-ID",
            ] {
                if part
                    .headers
                    .iter()
                    .filter(|header| header.get_key_ref().eq_ignore_ascii_case(name))
                    .count()
                    > 1
                {
                    self.fallback = Some("duplicate_mime_metadata");
                    return Ok(());
                }
            }
            let cte = part
                .headers
                .get_first_value("Content-Transfer-Encoding")
                .map(|v| v.trim().to_ascii_lowercase())
                .unwrap_or_else(|| "7bit".into());
            let disposition = part
                .headers
                .get_first_value("Content-Disposition")
                .map(|v| {
                    v.split(';')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_ascii_lowercase()
                })
                .filter(|value| !value.is_empty());
            let filename = part.filename();
            has_boundary = part
                .ctype
                .params
                .get("boundary")
                .is_some_and(|value| !value.is_empty());
            let cid = part.headers.get_first_value("Content-ID");
            let conflicting_params = common.ty.params.as_ref().is_some_and(|params| {
                params.iter().any(|(key, value)| {
                    if key.contains('*') {
                        return false;
                    }
                    let key = key.to_ascii_lowercase();
                    if key == "charset" {
                        !part.ctype.charset.eq_ignore_ascii_case(value)
                    } else {
                        part.ctype.params.get(&key).map(String::as_str) != Some(value.as_ref())
                    }
                })
            });
            if conflicting_params
                || single
                    .and_then(|v| v.id.as_deref())
                    .is_some_and(|expected| cid.as_deref() != Some(expected))
                || !ambiguous && part.is_attachment() != meta.attachment
                || part.ctype.mimetype != meta.content_type
                || single.is_some() && cte != meta.encoding
                || common.disposition.is_some() && disposition != meta.disposition
                || !ambiguous && filename != meta.filename
            {
                self.fallback = Some("bodystructure_header_conflict");
                return Ok(());
            }
            meta.attachment = part.is_attachment();
            meta.content_type = part.ctype.mimetype.clone();
            meta.encoding = cte;
            meta.filename = filename;
            meta.disposition = disposition;
            meta.cid = cid;
            meta.charset = std::mem::take(&mut part.ctype.charset);
            meta.start = part.ctype.params.remove("start");
            if root {
                self.budget.grow(&mut self.body.root_headers, raw.len())?;
                self.body.root_headers.extend_from_slice(raw);
            }
        } else {
            self.needed.push(HeaderRequest {
                section,
                root_headers: root,
                reason: if single.is_none() {
                    "multipart_headers_required"
                } else {
                    "classification_headers_required"
                },
            });
            return Ok(());
        }
        let node_id = self.body.nodes.len();
        self.budget.grow(&mut self.body.nodes, 1)?;
        self.body.nodes.push(BodyNode {
            parent,
            children: vec![],
            kind: BodyKind::Leaf(self.parts.len()),
            nonempty: None,
        });
        if let Some(parent) = parent {
            self.budget.grow(&mut self.body.nodes[parent].children, 1)?;
            self.body.nodes[parent].children.push(node_id);
        }
        if let BodyStructure::Multipart { bodies, .. } = node {
            if meta.attachment {
                self.fallback = Some("whole_multipart_attachment");
                return Ok(());
            }
            if supported(&meta.encoding) {
                if !matches!(meta.encoding.as_str(), "7bit" | "8bit" | "binary") || !has_boundary {
                    self.fallback = Some("multipart_shape_unverified");
                    return Ok(());
                }
                if bodies.is_empty() {
                    self.fallback = Some("empty_multipart_shape");
                    return Ok(());
                }
                self.budget.section(&section)?;
                self.budget.owned(&meta.content_type)?;
                self.budget.owned(&meta.encoding)?;
                self.budget.owned_optional(&meta.cid)?;
                self.budget.owned_optional(&meta.start)?;
                self.body.nodes[node_id].kind = BodyKind::Container {
                    section,
                    media_type: meta.content_type,
                    encoding: meta.encoding,
                    content_id: meta.cid,
                    related_start: meta.start,
                };
                for (i, child) in bodies.iter().enumerate() {
                    self.visit(
                        child,
                        match &self.body.nodes[node_id].kind {
                            BodyKind::Container { section, .. } => section.child(i)?,
                            _ => return Err(MapError::StructureLimit),
                        },
                        false,
                        common.ty.subtype.eq_ignore_ascii_case("digest"),
                        Some(node_id),
                    )?;
                }
                return Ok(());
            }
        } else if meta.content_type.starts_with("multipart/") {
            self.fallback = Some("multipart_shape_conflict");
            return Ok(());
        }
        let ordinal = if meta.attachment {
            let value = self.attachments;
            self.attachments += 1;
            Some(value)
        } else {
            None
        };
        for text in [&meta.content_type, &meta.encoding, &meta.charset] {
            self.budget.owned(text)?;
        }
        for text in [&meta.disposition, &meta.filename, &meta.cid] {
            self.budget.owned_optional(text)?;
        }
        self.budget.section(&section)?;
        self.budget.grow(&mut self.parts, 1)?;
        let downloadable = supported(&meta.encoding);
        self.parts.push(ManifestPart {
            section,
            attachment_index: self.parts.len(),
            attachment_ordinal: ordinal,
            is_attachment: meta.attachment,
            content_type: meta.content_type,
            charset: meta.charset,
            transfer_encoding: meta.encoding,
            encoded_size: single.map(|value| value.octets),
            disposition: meta.disposition,
            filename: meta.filename,
            content_id: meta.cid,
            downloadable,
        });
        Ok(())
    }
}
#[cfg(test)]
mod tests;

#[cfg(test)]
#[path = "mapping/body_selection_tests.rs"]
mod body_selection_tests;
