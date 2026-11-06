//! Resource accounting before MIME object creation and transfer/charset decoding.
//! mailparse's tree entry point cannot enforce a global part/header budget. Keep
//! structure parsing here, and use its header and leaf codecs only on bounded input.
pub(crate) mod selection;

use crate::error::{LimitKind, MailError, Result};
use mailparse::{MailHeader, MailHeaderMap, ParsedContentType, body::Body};

pub(crate) const MAX_PARTS: usize = 1024;
pub(crate) const MAX_DEPTH: usize = 32;
const MAX_HEADER_BLOCK: usize = 128 * 1024;
const MAX_HEADER_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_HEADER_LINES: usize = 8192;
const MAX_DECODE_BYTES: usize = 32 * 1024 * 1024;

pub(crate) fn attachment_input_limit(decoded_limit: usize) -> usize {
    decoded_limit
        .saturating_mul(2)
        .saturating_add(4096)
        .min(MAX_DECODE_BYTES)
}

#[derive(Default)]
struct Budget {
    parts: usize,
    header_bytes: usize,
    header_lines: usize,
}

pub(crate) struct Part<'a> {
    pub headers: Vec<MailHeader<'a>>,
    pub ctype: ParsedContentType,
    pub subparts: Vec<Part<'a>>,
    body: &'a [u8],
}

fn headers_with_budget<'a>(
    raw: &'a [u8],
    budget: &mut Budget,
    max_block: usize,
) -> Result<(Vec<MailHeader<'a>>, usize)> {
    // Scan without allocating, including folded lines. Never hand an unbounded
    // header section to parse_headers or its encoded-word/parameter decoders.
    let mut end = 0;
    for line in raw.split_inclusive(|b| *b == b'\n') {
        end += line.len();
        budget.header_bytes += line.len();
        budget.header_lines += 1;
        if end > max_block
            || budget.header_bytes > MAX_HEADER_BYTES
            || budget.header_lines > MAX_HEADER_LINES
        {
            return Err(MailError::limit_kind(LimitKind::HeaderBytes));
        }
        if line == b"\n" || line == b"\r\n" {
            break;
        }
    }
    mailparse::parse_headers(&raw[..end]).map_err(|_| MailError::backend())
}

pub(crate) fn headers(raw: &[u8]) -> Result<Vec<MailHeader<'_>>> {
    Ok(headers_with_budget(raw, &mut Budget::default(), MAX_HEADER_BLOCK)?.0)
}

/// List/search accepts a larger top-level HEADER sample; other MIME readers
/// keep their normal per-block limit. The byte and line preflight is shared.
pub(crate) fn summary_headers(raw: &[u8], max_block: usize) -> Result<Vec<MailHeader<'_>>> {
    Ok(headers_with_budget(raw, &mut Budget::default(), max_block)?.0)
}

pub(crate) fn parse(raw: &[u8]) -> Result<Part<'_>> {
    parse_part(raw, &mut Budget::default(), 0, false)
}

/// Inspect wire headers, never decoded RFC 2047 values or body text. Embedded
/// identity-encoded messages share the same structural/header budgets.
pub(crate) fn requires_smtputf8(raw: &[u8]) -> Result<bool> {
    if raw.is_ascii() {
        return Ok(false);
    }
    fn inspect(part: &Part<'_>, budget: &mut Budget, depth: usize) -> Result<bool> {
        if part
            .headers
            .iter()
            .any(|header| !header.get_key_raw().is_ascii() || !header.get_value_raw().is_ascii())
        {
            return Ok(true);
        }
        for child in &part.subparts {
            if inspect(child, budget, depth + 1)? {
                return Ok(true);
            }
        }
        if matches!(
            part.ctype.mimetype.as_str(),
            "message/rfc822" | "message/global"
        ) && !part.body.is_empty()
            && part
                .headers
                .get_first_value("Content-Transfer-Encoding")
                .is_none_or(|value| {
                    matches!(
                        value.trim().to_ascii_lowercase().as_str(),
                        "7bit" | "8bit" | "binary"
                    )
                })
        {
            let nested = parse_part(part.body, budget, depth + 1, false)?;
            return inspect(&nested, budget, depth + 1);
        }
        Ok(false)
    }
    let mut budget = Budget::default();
    let part = parse_part(raw, &mut budget, 0, false)?;
    inspect(&part, &mut budget, 0)
}

fn parse_part<'a>(
    raw: &'a [u8],
    budget: &mut Budget,
    depth: usize,
    digest: bool,
) -> Result<Part<'a>> {
    if depth > MAX_DEPTH || budget.parts >= MAX_PARTS {
        return Err(MailError::limit_kind(LimitKind::MimeStructure));
    }
    budget.parts += 1;
    let (headers, body_start) = headers_with_budget(raw, budget, MAX_HEADER_BLOCK)?;
    let ctype = mailparse::parse_content_type(
        &headers.get_first_value("Content-Type").unwrap_or_else(|| {
            if digest {
                "message/rfc822"
            } else {
                "text/plain"
            }
            .into()
        }),
    );
    let mut part = Part {
        headers,
        ctype,
        subparts: Vec::new(),
        body: &raw[body_start..],
    };
    // An unknown transfer encoding makes the entire entity opaque (RFC 2045
    // section 6.4), even when its declared type looks like a multipart tree.
    if part.transfer_encoding().is_err() {
        return Ok(part);
    }
    if part.ctype.mimetype.starts_with("multipart/")
        && let Some(boundary) = part.ctype.params.get("boundary")
    {
        if boundary.is_empty() || boundary.len() > 200 || boundary.contains(['\r', '\n']) {
            return Err(MailError::limit_kind(LimitKind::MimeStructure));
        }
        let marker = format!("--{boundary}");
        let body = part.body;
        let mut offset = 0;
        let mut start = None;
        for line in body.split_inclusive(|b| *b == b'\n') {
            let end = offset + line.len();
            let line = line.strip_suffix(b"\n").unwrap_or(line);
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if let Some(tail) = line.strip_prefix(marker.as_bytes()) {
                let closing = tail.starts_with(b"--");
                let tail = if closing { &tail[2..] } else { tail };
                if tail.iter().all(|b| matches!(b, b' ' | b'\t')) {
                    if let Some(start) = start {
                        let mut stop = offset;
                        if stop > start && body[stop - 1] == b'\n' {
                            stop -= 1;
                            if stop > start && body[stop - 1] == b'\r' {
                                stop -= 1;
                            }
                        }
                        // Charge the child before parsing headers or growing the tree.
                        part.subparts.push(parse_part(
                            &body[start..stop],
                            budget,
                            depth + 1,
                            part.ctype.mimetype == "multipart/digest",
                        )?);
                    }
                    start = Some(end);
                    if closing {
                        return Ok(part);
                    }
                }
            }
            offset = end;
        }
        // Permit a missing final boundary, as mailparse does.
        if let Some(start) = start {
            part.subparts.push(parse_part(
                &body[start..],
                budget,
                depth + 1,
                part.ctype.mimetype == "multipart/digest",
            )?);
        }
    }
    Ok(part)
}

impl Part<'_> {
    pub fn filename(&self) -> Option<String> {
        self.get_content_disposition()
            .params
            .get("filename")
            .cloned()
            .or_else(|| self.ctype.params.get("name").cloned())
    }

    pub fn is_attachment(&self) -> bool {
        // mailparse leaves RFC 2231 keys encoded when the charset is unknown
        // or a continuation cannot be assembled. A failed filename decode is
        // still attachment intent, never permission to include it in the body.
        let has_name = |params: &std::collections::BTreeMap<String, String>, base: &str| {
            params.keys().any(|key| {
                key == base
                    || key
                        .strip_prefix(base)
                        .is_some_and(|tail| tail.starts_with('*'))
            })
        };
        let disposition = self.get_content_disposition();
        // RFC 2183 section 2.8: types we do not support as inline are
        // attachments. mailparse represents an absent/empty type as
        // Extension("") rather than Inline; preserve that empty case.
        let attached_disposition = match &disposition.disposition {
            mailparse::DispositionType::Inline => false,
            mailparse::DispositionType::Extension(value) => !value.trim().is_empty(),
            _ => true,
        };
        attached_disposition
            || has_name(&disposition.params, "filename")
            || has_name(&self.ctype.params, "name")
    }

    pub fn get_content_disposition(&self) -> mailparse::ParsedContentDisposition {
        mailparse::parse_content_disposition(
            &self
                .headers
                .get_first_value("Content-Disposition")
                .unwrap_or_default(),
        )
    }

    fn transfer_encoding(&self) -> Result<Option<String>> {
        let encoding = self
            .headers
            .get_first_value("Content-Transfer-Encoding")
            .map(|value| value.trim().to_ascii_lowercase());
        match encoding.as_deref() {
            None | Some("7bit" | "8bit" | "binary" | "base64" | "quoted-printable") => Ok(encoding),
            _ => Err(crate::html::unsupported()),
        }
    }

    fn encoded(&self) -> Result<Body<'_>> {
        let encoding = self.transfer_encoding()?;
        // Body::new silently defaults unknown labels to SevenBit; never pass
        // those labels through or claim their bytes have been decoded.
        Ok(Body::new(self.body, &self.ctype, &encoding))
    }

    pub fn encoded_len(&self) -> usize {
        self.body.len()
    }

    pub(crate) fn encoded_body(&self) -> &[u8] {
        self.body
    }

    pub(crate) fn attachment_encoding_supported(&self) -> bool {
        self.transfer_encoding().is_ok()
    }

    pub(crate) fn attachment_size_hint(&self) -> (Option<usize>, &'static str) {
        match self
            .transfer_encoding()
            .as_ref()
            .map(|value| value.as_deref())
        {
            Ok(None | Some("7bit" | "8bit" | "binary")) => (Some(self.body.len()), "exact"),
            Ok(Some("base64")) => {
                let octets = self
                    .body
                    .iter()
                    .filter(|b| !b.is_ascii_whitespace())
                    .count();
                let padding = self
                    .body
                    .iter()
                    .rev()
                    .filter(|b| !b.is_ascii_whitespace())
                    .take_while(|b| **b == b'=')
                    .take(2)
                    .count();
                (
                    Some((octets / 4 * 3 + octets % 4 * 3 / 4).saturating_sub(padding)),
                    "estimate",
                )
            }
            _ => (None, "unknown"),
        }
    }

    pub fn body_raw(&self, max: usize) -> Result<Vec<u8>> {
        // Transfer codecs never expand beyond encoded bytes. Limit their input
        // before they allocate cleaning buffers or decoded output.
        if self.body.len() > attachment_input_limit(max) {
            return Err(MailError::limit_kind(LimitKind::DecodedAttachmentBytes));
        }
        let decoded = match self.encoded()? {
            Body::Base64(b) | Body::QuotedPrintable(b) => b.get_decoded(),
            Body::SevenBit(b) | Body::EightBit(b) => Ok(b.get_raw().to_vec()),
            Body::Binary(b) => Ok(b.get_raw().to_vec()),
        }
        .map_err(|_| MailError::backend())?;
        if decoded.len() > max {
            return Err(MailError::limit_with(
                LimitKind::DecodedAttachmentBytes,
                max,
                decoded.len(),
            ));
        }
        Ok(decoded)
    }

    pub fn plain_body(&self) -> Result<String> {
        Ok(self.display_body(crate::html::HtmlDisplay::Legacy)?.text)
    }

    pub fn display_body(
        &self,
        format: crate::html::HtmlDisplay,
    ) -> Result<crate::html::DisplayBody> {
        let mut out = String::new();
        let mut remaining = MAX_DECODE_BYTES;
        let mut context = crate::html::DisplayContext::new(format);
        self.append_plain(&mut out, &mut remaining, &mut context)?;
        Ok(crate::html::DisplayBody {
            text: out,
            metadata: context.into_metadata(),
        })
    }

    fn append_plain(
        &self,
        out: &mut String,
        remaining: &mut usize,
        context: &mut crate::html::DisplayContext,
    ) -> Result<()> {
        self.project(out, remaining, context, false).map(|_| ())
    }

    pub(crate) fn decoded_display_leaves(
        &self,
        remaining: &mut usize,
        consume: impl FnMut(String, bool, usize) -> Result<()>,
    ) -> Result<()> {
        self.display_body_with_leaves(crate::html::HtmlDisplay::Legacy, remaining, consume)
            .map(|_| ())
    }

    pub(crate) fn display_body_with_leaves(
        &self,
        format: crate::html::HtmlDisplay,
        remaining: &mut usize,
        mut consume: impl FnMut(String, bool, usize) -> Result<()>,
    ) -> Result<crate::html::DisplayBody> {
        let mut text = String::new();
        let mut context = crate::html::DisplayContext::new(format);
        let leaves = self.project(&mut text, remaining, &mut context, true)?;
        for (text, html, reserve) in leaves {
            consume(text, html, reserve)?;
        }
        Ok(crate::html::DisplayBody {
            text,
            metadata: context.into_metadata(),
        })
    }

    fn project(
        &self,
        out: &mut String,
        remaining: &mut usize,
        context: &mut crate::html::DisplayContext,
        retain: bool,
    ) -> Result<Vec<(String, bool, usize)>> {
        let mut rejected = std::collections::HashSet::new();
        let mut work = crate::html::RenderWork::default();
        let format = context.format();
        let start = out.len();
        'candidate: loop {
            let mut selected = Vec::new();
            // Raw entities always have known encoded emptiness.
            if selection::select(self, &mut selected, true, &|part| {
                rejected.contains(&std::ptr::from_ref(part))
            })
            .unwrap_or(true)
            {
                return Err(crate::html::unsupported());
            }
            let mut leaves = Vec::new();
            for part in selected {
                let html = part.ctype.mimetype == "text/html";
                let (text, reserve) = part.decode_text(remaining)?;
                if let Err(error) = crate::html::project_body_leaf(
                    out, &text, html, reserve, remaining, context, &mut work,
                ) {
                    if error.code != "UNSUPPORTED_BODY" {
                        return Err(error);
                    }
                    rejected.insert(std::ptr::from_ref(part));
                    out.truncate(start);
                    *context = crate::html::DisplayContext::new(format);
                    continue 'candidate;
                }
                if retain {
                    leaves.push((text, html, reserve));
                }
            }
            return Ok(leaves);
        }
    }

    fn decode_text(&self, remaining: &mut usize) -> Result<(String, usize)> {
        // Transfer output never exceeds input. UTF-8 can require up to
        // four bytes per input byte, charged across all attempted leaves.
        let limit = || MailError::limit_kind(LimitKind::DecodedBodyBytes);
        let reserve = self.body.len().checked_mul(4).ok_or_else(limit)?;
        *remaining = remaining.checked_sub(reserve).ok_or_else(limit)?;
        let text = match self.encoded()? {
            Body::Base64(b) | Body::QuotedPrintable(b) => b.get_decoded_as_string(),
            Body::SevenBit(b) | Body::EightBit(b) => b.get_as_string(),
            Body::Binary(b) => b.get_as_string(),
        }
        .map_err(|_| MailError::backend())?;
        if text.len() > reserve {
            return Err(limit());
        }
        Ok((text, reserve))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_encodings_require_known_decoders_before_interpretation() {
        for (encoding, body) in [
            (None, "hello"),
            (Some("7bit"), "hello"),
            (Some("8BIT"), "hello"),
            (Some("binary"), "hello"),
            (Some("BaSe64"), "aGVsbG8="),
            (Some("Quoted-Printable"), "hel=6Co"),
        ] {
            let header = encoding.map_or_else(String::new, |value| {
                format!("Content-Transfer-Encoding: {value}\r\n")
            });
            let raw = format!("Content-Type: text/plain; charset=utf-8\r\n{header}\r\n{body}");
            let part = parse(raw.as_bytes()).unwrap();
            assert_eq!(part.plain_body().unwrap(), "hello");
            assert_eq!(part.body_raw(1024).unwrap(), b"hello");
        }
        let raw = b"Content-Type: text/plain\nContent-Transfer-Encoding: x-example\n\nencoded";
        let part = parse(raw).unwrap();
        assert_eq!(part.plain_body().unwrap_err().code, "UNSUPPORTED_BODY");
        assert_eq!(part.body_raw(1024).unwrap_err().code, "UNSUPPORTED_BODY");
        let raw = b"Content-Type: multipart/mixed; boundary=m\nContent-Transfer-Encoding: x-example\n\n--m\nContent-Type: text/plain\n\nnot decoded\n--m--\n";
        let opaque = parse(raw).unwrap();
        assert!(opaque.subparts.is_empty());
        assert_eq!(opaque.plain_body().unwrap_err().code, "UNSUPPORTED_BODY");
        let raw = b"Content-Type: multipart/alternative; boundary=a\n\n--a\nContent-Type: text/plain\n\nComplete fallback\n--a\nContent-Type: multipart/mixed; boundary=m\n\n--m\nContent-Type: text/plain\n\nSignature\n--m\nContent-Type: text/plain\nContent-Transfer-Encoding: x-example\n\nencoded\n--m--\n--a--\n";
        assert_eq!(
            parse(raw).unwrap().plain_body().unwrap(),
            "Complete fallback"
        );
    }

    #[test]
    fn plain_probes_require_all_inline_parts_and_prune_only_attachments() {
        for (candidate, expected) in [
            ("Content-Type: text/html\n\n<p>Invoice 123</p>".to_owned(), "Complete fallback"),
            ("Content-Type: multipart/related; boundary=r\n\n--r\nContent-Type: text/html\n\n<p>Invoice 123</p>\n--r\n\nRESOURCE_ONLY\n--r--".to_owned(), "Complete fallback"),
            ("Content-Type: multipart/alternative; boundary=n\n\n--n\nContent-Type: text/html\n\n<p>Invoice 123</p>\n--n--".to_owned(), "Complete fallback"),
            ("Content-Type: text/html\n\n".to_owned(), "Signature"),
            ("Content-Type: text/html\nContent-Disposition: attachment\n\n<p>ATTACHMENT_ONLY</p>".to_owned(), "Signature"),
            ("Content-Type: text/html; name=notes.html\n\n<p>ATTACHMENT_ONLY</p>".to_owned(), "Signature"),
        ] {
            let raw = format!("Content-Type: multipart/alternative; boundary=a\n\n--a\n\nComplete fallback\n--a\nContent-Type: multipart/mixed; boundary=m\n\n--m\n\nSignature\n--m\n{candidate}\n--m--\n--a--\n");
            assert_eq!(parse(raw.as_bytes()).unwrap().plain_body().unwrap(), expected);
        }
        // Failed later candidates cannot leak their signature into a genuinely
        // empty complete plain representation.
        let raw = b"Content-Type: multipart/alternative; boundary=a\n\n--a\nContent-Type: text/plain\n\n\n--a\nContent-Type: multipart/mixed; boundary=m\n\n--m\n\nSignature\n--m\nContent-Type: text/html\n\n<p>Invoice 123</p>\n--m--\n--a--\n";
        assert_eq!(parse(raw).unwrap().plain_body().unwrap(), "");
    }

    #[test]
    fn alternatives_require_supported_charset_and_complete_html() {
        let plain = "Content-Type: text/plain; charset=utf-8\n\nComplete fallback";
        let html = "Content-Type: text/html\n\n<p>Complete fallback</p>";
        let unknown = "Content-Type: text/plain; charset=x-unknown\n\nUnreadable";
        let foreign = "Content-Type: text/html\n\nVisible<svg><![CDATA[not HTML]]></svg>";
        let mixed = format!(
            "Content-Type: multipart/mixed; boundary=m\n\n--m\n\nSignature\n--m\n{foreign}\n--m--"
        );
        for (fallback, preferred) in [(plain, unknown), (html, &mixed)] {
            let raw = format!(
                "Content-Type: multipart/alternative; boundary=a\n\n--a\n{fallback}\n--a\n{preferred}\n--a--\n"
            );
            assert_eq!(
                parse(raw.as_bytes()).unwrap().plain_body().unwrap(),
                "Complete fallback"
            );
        }
        let raw = format!(
            "Content-Type: multipart/alternative; boundary=a\n\n--a\n{unknown}\n--a\n{foreign}\n--a--\n"
        );
        assert_eq!(
            parse(raw.as_bytes())
                .unwrap()
                .plain_body()
                .unwrap_err()
                .code,
            "UNSUPPORTED_BODY"
        );
        let raw = format!(
            "Content-Type: multipart/alternative; boundary=a\n\n--a\n{html}\n--a\n{foreign}\n--a--\n"
        );
        let mut out = String::new();
        let mut remaining = foreign.split_once("\n\n").unwrap().1.len() * 4;
        assert_eq!(
            parse(raw.as_bytes())
                .unwrap()
                .append_plain(
                    &mut out,
                    &mut remaining,
                    &mut crate::html::DisplayContext::default(),
                )
                .unwrap_err()
                .code,
            "LIMIT_EXCEEDED"
        );
        assert!(out.is_empty());
    }

    #[test]
    fn headers_and_decode_are_bounded_before_allocation() {
        let bytes = vec![b'x'; MAX_HEADER_BLOCK + 1];
        assert_eq!(headers(&bytes).unwrap_err().code, "LIMIT_EXCEEDED");
        let p = parse(b"\nhello").unwrap();
        let mut out = String::new();
        let mut remaining = 19;
        assert_eq!(
            p.append_plain(
                &mut out,
                &mut remaining,
                &mut crate::html::DisplayContext::default()
            )
            .unwrap_err()
            .code,
            "LIMIT_EXCEEDED"
        );
        assert!(out.is_empty());
        let mut remaining = 20;
        p.append_plain(
            &mut out,
            &mut remaining,
            &mut crate::html::DisplayContext::default(),
        )
        .unwrap();
        assert_eq!(out, "hello");
        assert_eq!(remaining, 0);
        assert!(
            p.append_plain(
                &mut out,
                &mut remaining,
                &mut crate::html::DisplayContext::default()
            )
            .is_err()
        );
    }
}
