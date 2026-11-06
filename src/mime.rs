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
        self.decoded_display_leaves(remaining, |text, html, reserve| {
            let text = if html {
                crate::html::render_selected(&text, context)?
            } else {
                text
            };
            // Charset decoding is charged above. Readable HTML can add safe
            // indentation/address bytes beyond encoded*4; its shared formatter
            // independently enforces the 1 MiB output cap across selected leaves.
            if text.len() > reserve
                && !(html && context.format() == crate::html::HtmlDisplay::Readable)
            {
                return Err(MailError::limit_kind(LimitKind::DecodedBodyBytes));
            }
            out.push_str(&text);
            Ok(())
        })
    }

    pub(crate) fn decoded_display_leaves(
        &self,
        remaining: &mut usize,
        mut consume: impl FnMut(String, bool, usize) -> Result<()>,
    ) -> Result<()> {
        let mut selected = Vec::new();
        // Raw entities always have known encoded emptiness.
        if selection::select(self, &mut selected, true).unwrap_or(true) {
            return Err(crate::html::unsupported());
        }
        for part in selected {
            let html = part.ctype.mimetype == "text/html";
            if !html && part.ctype.mimetype != "text/plain" {
                continue;
            }
            if !part.body.is_empty()
                && charset::Charset::for_label_no_replacement(part.ctype.charset.as_bytes())
                    .is_none()
            {
                return Err(crate::html::unsupported());
            }
            // Transfer output never exceeds input. UTF-8 can require up to
            // four bytes per input byte, charged across all selected leaves.
            let limit = || MailError::limit_kind(LimitKind::DecodedBodyBytes);
            let reserve = part.body.len().checked_mul(4).ok_or_else(limit)?;
            *remaining = remaining.checked_sub(reserve).ok_or_else(limit)?;
            let text = match part.encoded()? {
                Body::Base64(b) | Body::QuotedPrintable(b) => b.get_decoded_as_string(),
                Body::SevenBit(b) | Body::EightBit(b) => b.get_as_string(),
                Body::Binary(b) => b.get_as_string(),
            }
            .map_err(|_| MailError::backend())?;
            if text.len() > reserve {
                return Err(limit());
            }
            consume(text, html, reserve)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::html::HtmlDisplay;

    fn html_mixed(parts: &[String]) -> Vec<u8> {
        let mut raw = String::from("Content-Type: multipart/mixed; boundary=r03\r\n\r\n");
        for body in parts {
            raw.push_str("--r03\r\nContent-Type: text/html; charset=utf-8\r\n\r\n");
            raw.push_str(body);
            raw.push_str("\r\n");
        }
        raw.push_str("--r03--\r\n");
        raw.into_bytes()
    }

    #[test]
    fn readable_html_keeps_shared_candidate_selection_and_legacy_forward_text() {
        let raw = b"Content-Type: text/html; charset=utf-8\r\n\r\n<p><a href='https://example.test/order'>Order</a></p><ul><li>One</li><li>Two</li></ul>";
        let parsed = super::parse(raw).unwrap();
        let legacy = parsed.plain_body().unwrap();
        assert_eq!(legacy, "Order\nOne\nTwo");
        let readable = parsed.display_body(HtmlDisplay::Readable).unwrap();
        assert_eq!(
            readable.text,
            "Order (https://example.test/order)\n- One\n- Two"
        );
        assert_eq!(readable.metadata.format_version, "readable-v1");
        assert_eq!(readable.metadata.html_parts, 1);
        assert_eq!(parsed.plain_body().unwrap(), legacy);
        let raw = b"Content-Type: multipart/alternative; boundary=a\r\n\r\n--a\r\nContent-Type: text/plain\r\n\r\n\r\n--a\r\nContent-Type: text/html\r\n\r\n<p>HTML must not replace empty plain</p>\r\n--a--\r\n";
        let readable = super::parse(raw)
            .unwrap()
            .display_body(HtmlDisplay::Readable)
            .unwrap();
        assert_eq!(readable.text, "");
        assert_eq!(readable.metadata.html_parts, 0);
        let raw = b"Content-Type: multipart/alternative; boundary=a\r\n\r\n--a\r\n\r\nComplete fallback\r\n--a\r\nContent-Type: multipart/mixed; boundary=m\r\n\r\n--m\r\n\r\nPartial signature\r\n--m\r\nContent-Type: text/html\r\n\r\n<p>HTML candidate</p>\r\n--m--\r\n--a--\r\n";
        assert_eq!(
            super::parse(raw)
                .unwrap()
                .display_body(HtmlDisplay::Readable)
                .unwrap()
                .text,
            "Complete fallback"
        );
    }

    #[test]
    fn readable_html_shares_link_token_and_output_budgets_across_selected_leaves() {
        let links = |start: usize, end: usize| {
            (start..end)
                .map(|i| format!("<a href='https://example.test/{i}'>item{i}</a> "))
                .collect::<String>()
        };
        let raw = html_mixed(&[links(0, 200), links(200, 300)]);
        let body = super::parse(&raw)
            .unwrap()
            .display_body(HtmlDisplay::Readable)
            .unwrap();
        assert_eq!(body.metadata.html_parts, 2);
        assert_eq!(body.text.matches(" (https://").count(), 256);
        assert_eq!(
            body.metadata
                .omissions
                .iter()
                .find(|item| item.reason == "link_limit")
                .unwrap()
                .count,
            44
        );

        let raw = html_mixed(&["<b></b>".repeat(9000), "<b></b>".repeat(9000)]);
        let parsed = super::parse(&raw).unwrap();
        assert_eq!(parsed.plain_body().unwrap(), "");
        assert_eq!(
            parsed.display_body(HtmlDisplay::Readable).unwrap_err().code,
            "LIMIT_EXCEEDED"
        );

        let raw = html_mixed(&["a".repeat(600_000), "b".repeat(600_000)]);
        let parsed = super::parse(&raw).unwrap();
        assert_eq!(parsed.plain_body().unwrap().len(), 1_200_000);
        assert_eq!(
            parsed.display_body(HtmlDisplay::Readable).unwrap_err().code,
            "LIMIT_EXCEEDED"
        );
    }

    #[test]
    fn readable_html_expansion_uses_display_budget_after_bounded_charset_decode() {
        let html = format!(
            "{}<pre>{}</pre>{}",
            "<blockquote>".repeat(16),
            "x\n".repeat(1000),
            "</blockquote>".repeat(16)
        );
        let raw = format!("Content-Type: text/html; charset=utf-8\r\n\r\n{html}");
        let parsed = super::parse(raw.as_bytes()).unwrap();
        let body = parsed.display_body(HtmlDisplay::Readable).unwrap();
        assert!(body.text.len() > html.len() * 4);
        assert_eq!(body.text, format!("{}x\n", "> ".repeat(16)).repeat(1000));
        let mut out = String::new();
        let mut remaining = html.len() * 4 - 1;
        let mut context = crate::html::DisplayContext::new(HtmlDisplay::Readable);
        let error = parsed
            .append_plain(&mut out, &mut remaining, &mut context)
            .unwrap_err();
        assert_eq!(
            serde_json::to_value(error).unwrap()["details"]["limit_kind"],
            "decoded_body_bytes"
        );
        assert!(out.is_empty());
        assert_eq!(context.into_metadata().html_parts, 0);
    }

    #[test]
    fn readable_html_preserves_foreign_and_unknown_encoding_failures() {
        for raw in [
            &b"Content-Type: text/html\r\n\r\nvisible<svg><![CDATA[secret]]></svg>"[..],
            &b"Content-Type: text/html\r\nContent-Transfer-Encoding: unknown\r\n\r\n<p>opaque</p>"[..],
            &b"Content-Type: multipart/mixed; boundary=x\r\nContent-Transfer-Encoding: unknown\r\n\r\n--x\r\nContent-Type: text/html\r\n\r\n<p>opaque</p>\r\n--x--\r\n"[..],
        ] {
            let parsed = super::parse(raw).unwrap();
            assert_eq!(parsed.plain_body().unwrap_err().code, "UNSUPPORTED_BODY");
            assert_eq!(parsed.display_body(HtmlDisplay::Readable).unwrap_err().code, "UNSUPPORTED_BODY");
        }
    }

    #[test]
    fn relaxed_parallel_attachment_decode_accepts_large_payloads() {
        use base64::Engine;
        let barrier = std::sync::Barrier::new(4);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let barrier = &barrier;
                scope.spawn(move || {
                    let encoded = base64::engine::general_purpose::STANDARD.encode(vec![42u8; 12 * 1024 * 1024]);
                    let raw = format!("Content-Type: application/octet-stream\r\nContent-Transfer-Encoding: base64\r\n\r\n{encoded}");
                    drop(encoded);
                    let part = parse(raw.as_bytes()).unwrap();
                    let decoded = part.body_raw(crate::config::DEFAULT_MAX_MESSAGE).unwrap();
                    barrier.wait();
                    assert_eq!(decoded.len(), 12 * 1024 * 1024);
                    assert!(decoded.iter().all(|b| *b == 42));
                });
            }
        });
    }
    use super::*;

    #[test]
    fn smtp_utf8_inspection_uses_raw_headers_and_shared_budgets() {
        for (raw, expected) in [
            (&b"Subject: =?UTF-8?B?5Lit5paH?=\r\n\r\nbody"[..], false),
            ("Subject: 中文\r\n\r\nbody".as_bytes(), true),
            ("Subject: ASCII\r\n\r\nSubject: 中文".as_bytes(), false),
            (&b"Content-Type: application/octet-stream\r\n\r\n\xff\xfe"[..], false),
            ("Content-Type: message/global\r\nContent-Transfer-Encoding: base64\r\n\r\nU3ViamVjdDog5Lit5paHDQoNCmJvZHk=".as_bytes(), false),
        ] {
            assert_eq!(requires_smtputf8(raw).unwrap(), expected);
        }
        let deep = format!(
            "{}Subject: ASCII\r\n\r\n中文 body\r\n",
            "Content-Type: message/rfc822\r\n\r\n".repeat(MAX_DEPTH + 1)
        );
        assert_eq!(
            requires_smtputf8(deep.as_bytes()).unwrap_err().code,
            "LIMIT_EXCEEDED"
        );
        let raw = format!("Subject: {}\r\n\r\n中文", "x".repeat(MAX_HEADER_BLOCK));
        assert_eq!(
            requires_smtputf8(raw.as_bytes()).unwrap_err().code,
            "LIMIT_EXCEEDED"
        );
    }
    #[test]
    fn disposition_classification_preserves_empty_inline_and_filename_rules() {
        for (header, expected) in [
            ("", false),
            ("Content-Disposition:", false),
            ("Content-Disposition: \t", false),
            ("Content-Disposition: inline", false),
            ("Content-Disposition: INLINE", false),
            ("Content-Disposition: inline; size=1", false),
            ("Content-Disposition: attachment", true),
            ("Content-Disposition: x-test", true),
            ("Content-Disposition: X-Test; size=1", true),
            ("Content-Disposition: form-data", true),
            ("Content-Disposition: ; filename=notes.txt", true),
            ("Content-Disposition: inline; filename=notes.txt", true),
            ("Content-Disposition: x-test; filename=notes.txt", true),
            ("Content-Type: text/plain; name=notes.txt", true),
        ] {
            let raw = format!("{header}\r\n\r\n");
            assert_eq!(
                parse(raw.as_bytes()).unwrap().is_attachment(),
                expected,
                "{header}"
            );
        }
    }
    #[test]
    fn alternative_and_related_select_one_representation() {
        let alternative = b"Content-Type: multipart/alternative; boundary=a\n\n--a\n\nolder\n--a\nContent-Type: text/html\n\n<p>HTML</p>\n--a\n\nnewer\n--a--\n";
        assert_eq!(parse(alternative).unwrap().plain_body().unwrap(), "newer");
        let related = b"Content-Type: multipart/related; boundary=a; start=\"<root>\"\n\n--a\nContent-ID: <resource>\n\nRESOURCE_ONLY\n--a\nContent-ID: <root>\n\nVisible body\n--a--\n";
        assert_eq!(
            parse(related).unwrap().plain_body().unwrap(),
            "Visible body"
        );
        let related_default = b"Content-Type: multipart/related; boundary=a\n\n--a\n\nVisible body\n--a\n\nRESOURCE_ONLY\n--a--\n";
        assert_eq!(
            parse(related_default).unwrap().plain_body().unwrap(),
            "Visible body"
        );
    }
    #[test]
    fn html_fallback_keeps_plain_preference_and_ignores_resources() {
        let raw = b"Content-Type: multipart/alternative; boundary=a\n\n--a\n\nPlain original\n--a\nContent-Type: text/html\n\n<p>HTML alternative</p>\n--a--\n";
        assert_eq!(parse(raw).unwrap().plain_body().unwrap(), "Plain original");
        let raw = b"Content-Type: multipart/alternative; boundary=a\n\n--a\n\n\n--a\nContent-Type: text/html\n\n<p>HTML alternative</p>\n--a--\n";
        assert_eq!(parse(raw).unwrap().plain_body().unwrap(), "");
        let raw = b"Content-Type: multipart/related; boundary=a\n\n--a\nContent-Type: text/html\n\n<p>HTML original</p>\n--a\n\nRESOURCE_ONLY\n--a--\n";
        assert_eq!(parse(raw).unwrap().plain_body().unwrap(), "HTML original");
        let raw = b"Content-Type: multipart/mixed; boundary=a\n\n--a\nContent-Type: text/html\n\n<p>HTML original</p>\n--a\nContent-Type: text/html; name=attachment.html\n\n<p>ATTACHMENT_ONLY</p>\n--a--\n";
        assert_eq!(parse(raw).unwrap().plain_body().unwrap(), "HTML original");
    }

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

    const NORMAL: &[u8] = b"Subject: example\r\nContent-Type: multipart/mixed; boundary=outer\r\n\r\n--outer\r\nContent-Type: multipart/alternative; boundary=inner\r\n\r\n--inner\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\nhello =E2=82=AC\r\n--inner\r\nContent-Type: text/html\r\n\r\n<p>hello</p>\r\n--inner--\r\n--outer\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=example.txt\r\nContent-Transfer-Encoding: base64\r\n\r\nYWJj\r\n--outer--\r\n";

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
    fn normal_multipart_and_codecs_match_mailparse() {
        let p = parse(NORMAL).unwrap();
        let original = mailparse::parse_mail(NORMAL).unwrap();
        assert_eq!(
            p.headers.get_first_value("Subject"),
            original.headers.get_first_value("Subject")
        );
        assert_eq!(p.plain_body().unwrap(), "hello €");
        assert_eq!(p.subparts.len(), original.subparts.len());
        let attachment = &p.subparts[1];
        assert_eq!(attachment.body_raw(3).unwrap(), b"abc");
        assert_eq!(
            attachment
                .get_content_disposition()
                .params
                .get("filename")
                .unwrap(),
            "example.txt"
        );
        assert_eq!(attachment.body_raw(2).unwrap_err().code, "LIMIT_EXCEEDED");
    }

    #[test]
    fn charges_structure_before_parsing_child_headers() {
        let mut budget = Budget {
            parts: MAX_PARTS,
            ..Default::default()
        };
        assert_eq!(
            parse_part(b"invalid header", &mut budget, 0, false)
                .err()
                .unwrap()
                .code,
            "LIMIT_EXCEEDED"
        );
        assert_eq!(budget.header_bytes, 0);
        let mut budget = Budget::default();
        assert!(parse_part(b"invalid header", &mut budget, MAX_DEPTH + 1, false).is_err());
        assert_eq!(budget.parts, 0);
        assert_eq!(budget.header_bytes, 0);
    }

    #[test]
    fn shared_budgets_and_recovery() {
        let mut budget = Budget {
            parts: MAX_PARTS - 2,
            ..Default::default()
        };
        assert_eq!(
            parse_part(NORMAL, &mut budget, 0, false)
                .err()
                .unwrap()
                .code,
            "LIMIT_EXCEEDED"
        );
        assert_eq!(budget.parts, MAX_PARTS);
        let mut budget = Budget {
            header_bytes: MAX_HEADER_BYTES,
            ..Default::default()
        };
        assert_eq!(
            headers_with_budget(b"Subject: a\n\n", &mut budget, MAX_HEADER_BLOCK)
                .unwrap_err()
                .code,
            "LIMIT_EXCEEDED"
        );
        let mut budget = Budget {
            header_lines: MAX_HEADER_LINES,
            ..Default::default()
        };
        assert!(headers_with_budget(b"X: y\n\n", &mut budget, MAX_HEADER_BLOCK).is_err());
        assert_eq!(parse(NORMAL).unwrap().plain_body().unwrap(), "hello €");
    }

    #[test]
    fn complete_parse_stops_at_shared_structure_limit() {
        let mut raw = String::from("Content-Type: multipart/mixed; boundary=bounded\n\n");
        for _ in 0..MAX_PARTS {
            raw.push_str("--bounded\n\nnormal\n");
        }
        raw.push_str("--bounded--\n");
        assert_eq!(parse(raw.as_bytes()).err().unwrap().code, "LIMIT_EXCEEDED");
        assert_eq!(parse(NORMAL).unwrap().plain_body().unwrap(), "hello €");
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

    #[test]
    fn budget_errors_identify_header_structure_attachment_and_body_resources() {
        let kind = |error: MailError| {
            serde_json::to_value(error).unwrap()["details"]["limit_kind"].clone()
        };
        assert_eq!(
            kind(headers(&vec![b'x'; MAX_HEADER_BLOCK + 1]).unwrap_err()),
            "header_bytes"
        );
        let mut budget = Budget {
            parts: MAX_PARTS,
            ..Default::default()
        };
        assert_eq!(
            kind(parse_part(b"invalid", &mut budget, 0, false).err().unwrap()),
            "mime_structure"
        );
        assert_eq!(budget.header_bytes, 0);
        let attachment = parse(
            b"Content-Type: application/octet-stream\r\nContent-Disposition: attachment\r\n\r\nabc",
        )
        .unwrap();
        let error = attachment.body_raw(2).unwrap_err();
        let details = serde_json::to_value(error).unwrap();
        assert_eq!(details["details"]["limit_kind"], "decoded_attachment_bytes");
        assert_eq!(details["details"]["actual"], 3);
        assert_eq!(details["details"]["limit"], 2);
        let encoded = parse(b"Content-Type: application/octet-stream\r\n\r\nxxxxxxxx").unwrap();
        assert_eq!(
            kind(encoded.body_raw(0).unwrap_err()),
            "decoded_attachment_bytes"
        );
        let body = parse(b"Content-Type: text/plain\r\n\r\nhello").unwrap();
        let (mut out, mut remaining) = (String::new(), 19);
        assert_eq!(
            kind(
                body.append_plain(
                    &mut out,
                    &mut remaining,
                    &mut crate::html::DisplayContext::default()
                )
                .unwrap_err()
            ),
            "decoded_body_bytes"
        );
        assert!(out.is_empty());
    }

    #[test]
    fn header_only_paths_ignore_body_structure() {
        assert_eq!(
            headers(NORMAL).unwrap().get_first_value("Subject").unwrap(),
            "example"
        );
        let raw = b"Subject: header only\n\nnot a MIME header\n";
        assert_eq!(headers(raw).unwrap().len(), 1);
    }

    #[test]
    fn boundaries_crlf_lf_padding_and_missing_close() {
        for newline in ["\r\n", "\n"] {
            for close in [true, false] {
                let raw = format!(
                    "Content-Type: multipart/mixed; boundary=\"x;y\"{newline}{newline}preamble{newline}--x;y \t{newline}{newline}hello{newline}{}",
                    if close {
                        format!("--x;y--\t{newline}epilogue")
                    } else {
                        String::new()
                    }
                );
                let p = parse(raw.as_bytes()).unwrap();
                assert_eq!(p.subparts.len(), 1);
                assert_eq!(
                    p.plain_body().unwrap(),
                    if close {
                        "hello".into()
                    } else {
                        format!("hello{newline}")
                    }
                );
            }
        }
    }
}
