//! Bounded encoded prefixes from the same MIME representation as full reads.
use super::{
    body::{ReadFault, decode_charset, decode_transfer},
    body_tree::{BodyKind, Node},
    charset_decode::{CharsetDecoder, DecodeError},
    fetch, reader,
    transfer_decode::{Decoder, Encoding},
};
use crate::{
    error::{LimitKind, MailError},
    html::{self, DisplayContext, HtmlDisplay},
    mime::{self, selection},
};
use mailparse::MailHeaderMap;
use tokio::io::{AsyncRead, AsyncWrite};

pub(in crate::mail) const PAGE_ENCODED_BYTES: usize = 128 * 1024;
const MESSAGE_ENCODED_BYTES: usize = 8 * 1024;
const DISPLAY_CODE_POINTS: usize = 200;

#[derive(Debug, Eq, PartialEq)]
pub(in crate::mail) enum Preview {
    Text { text: String, truncated: bool },
    BudgetSkipped,
}

struct SampleBudget<'a> {
    message: usize,
    page: &'a mut usize,
}

impl SampleBudget<'_> {
    fn available(&self, requested: usize) -> usize {
        requested.min(self.message).min(*self.page)
    }

    // Charge commands before sending them. Failed commands and zero-byte
    // responses (including EOF probes) do not refund the requested bytes.
    fn claim(&mut self, requested: usize) -> usize {
        let bytes = self.available(requested);
        self.message -= bytes;
        *self.page -= bytes;
        bytes
    }
}

struct SampleDecoder {
    charset: CharsetDecoder,
    transfer: Decoder,
    text: String,
}

impl SampleDecoder {
    fn new(charset: &str, encoding: &str, sample: usize) -> Result<Self, ReadFault> {
        let reserve = sample * 4;
        let charset =
            CharsetDecoder::new(charset.as_bytes(), true, reserve).map_err(charset_fault)?;
        let encoding = match encoding {
            "7bit" | "8bit" | "binary" => Encoding::Identity,
            "base64" => Encoding::Base64,
            "quoted-printable" => Encoding::QuotedPrintable,
            _ => return Err(ReadFault::local(html::unsupported())),
        };
        Ok(Self {
            charset,
            transfer: Decoder::new(encoding, reserve),
            text: String::new(),
        })
    }

    fn feed(&mut self, sample: &[u8]) -> Result<(), ReadFault> {
        decode_transfer(
            &mut self.transfer,
            &mut self.charset,
            sample,
            false,
            &mut self.text,
        )
        .map_err(ReadFault::local)
    }

    fn finish(mut self, complete: bool) -> Result<String, ReadFault> {
        if complete {
            decode_transfer(
                &mut self.transfer,
                &mut self.charset,
                &[],
                true,
                &mut self.text,
            )
            .map_err(ReadFault::local)?;
            decode_charset(&mut self.charset, &[], true, &mut self.text)
                .map_err(ReadFault::local)?;
        }
        Ok(self.text)
    }
}

#[derive(Default)]
struct PreviewText {
    text: String,
    points: usize,
    pending_space: bool,
}

impl PreviewText {
    fn append_leaf(
        &mut self,
        text: String,
        is_html: bool,
        complete: bool,
        display: &mut DisplayContext,
    ) -> Result<bool, ReadFault> {
        let rendered = if is_html {
            if complete {
                html::render_selected(&text, display)
            } else {
                html::render_selected_prefix(&text, display)
            }
            .map_err(ReadFault::local)?
        } else {
            text
        };
        Ok(self.append(&rendered) || !complete)
    }

    /// Return true if a non-whitespace character was dropped. Carry whitespace
    /// across leaf boundaries and omit leading or trailing separators.
    fn append(&mut self, text: &str) -> bool {
        for c in text.chars() {
            if c.is_whitespace() {
                self.pending_space = self.points != 0;
                continue;
            }
            let space = usize::from(self.pending_space);
            if self.points + space >= DISPLAY_CODE_POINTS {
                return true;
            }
            if self.pending_space {
                self.text.push(' ');
                self.pending_space = false;
            }
            self.text.push(c);
            self.points += space + 1;
        }
        false
    }

    fn finish(self, truncated: bool) -> Preview {
        Preview::Text {
            text: self.text,
            truncated,
        }
    }
}

fn manifest_fault(fault: reader::FetchFailure) -> ReadFault {
    if matches!(
        fault,
        reader::FetchFailure::Fallback {
            synchronized: true,
            ..
        }
    ) {
        ReadFault::local(fault.mail())
    } else {
        ReadFault::command(fault.mail())
    }
}

fn charset_fault(error: DecodeError) -> ReadFault {
    ReadFault::local(match error {
        DecodeError::UnsupportedCharset | DecodeError::RawFallbackUtf7 => html::unsupported(),
        DecodeError::Utf8Limit { .. } => MailError::limit_kind(LimitKind::DecodedBodyBytes),
        _ => MailError::backend(),
    })
}

/// The caller owns the selected mailbox, page deadline and shared page budget.
/// Only MIME body slices and probes are charged here. Metadata retains the
/// manifest reader's own header, structure, command and transport limits.
pub(in crate::mail) async fn read<S>(
    session: &mut async_imap::Session<S>,
    uid: u32,
    page_remaining: &mut usize,
) -> Result<Preview, ReadFault>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
{
    if *page_remaining == 0 {
        return Ok(Preview::BudgetSkipped);
    }
    let mut budget = SampleBudget {
        message: MESSAGE_ENCODED_BYTES,
        page: page_remaining,
    };
    let mut manifest = reader::manifest(session, uid)
        .await
        .map_err(manifest_fault)?;
    let selected = loop {
        let mut selected = Vec::new();
        match selection::select(
            Node {
                manifest: &manifest,
                index: 0,
            },
            &mut selected,
            true,
        ) {
            Ok(true) => return Err(ReadFault::local(html::unsupported())),
            Ok(false) => {
                break selected
                    .into_iter()
                    .map(|node| node.index)
                    .collect::<Vec<_>>();
            }
            Err(node) => {
                if budget.claim(1) == 0 {
                    return Ok(Preview::BudgetSkipped);
                }
                let index = node.index;
                let section = match &manifest.body.nodes[index].kind {
                    BodyKind::Leaf(part) => &manifest.parts[*part].section,
                    BodyKind::Container { section, .. } => section,
                };
                let bytes = fetch::section_chunk(session, uid, section, 0, 1, None)
                    .await
                    .map_err(ReadFault::command)?;
                manifest.body.nodes[index].nonempty = Some(!bytes.is_empty());
            }
        }
    };
    let mut output = PreviewText::default();
    let mut display = DisplayContext::new(HtmlDisplay::Readable);
    for node in selected {
        let BodyKind::Leaf(index) = manifest.body.nodes[node].kind else {
            return Err(ReadFault::local(MailError::backend()));
        };
        let part = &manifest.parts[index];
        let encoded = part
            .encoded_size
            .ok_or_else(|| ReadFault::local(MailError::backend()))? as usize;
        let sample = budget.available(encoded);
        if sample == 0 && encoded != 0 {
            return Ok(output.finish(true));
        }
        let (leaf, complete) = if encoded == 0 {
            if budget.claim(1) == 0 {
                return Ok(output.finish(true));
            }
            fetch::section_chunk(session, uid, &part.section, 0, 1, Some(0))
                .await
                .map_err(ReadFault::command)?;
            (String::new(), true)
        } else {
            let mut decoder = SampleDecoder::new(&part.charset, &part.transfer_encoding, sample)?;
            budget.claim(sample);
            let bytes = fetch::section_chunk(session, uid, &part.section, 0, sample, Some(sample))
                .await
                .map_err(ReadFault::command)?;
            decoder.feed(&bytes)?;
            let complete = sample == encoded && budget.claim(1) != 0;
            if complete {
                fetch::section_chunk(session, uid, &part.section, encoded, 1, Some(0))
                    .await
                    .map_err(ReadFault::command)?;
            }
            (decoder.finish(complete)?, complete)
        };
        if output.append_leaf(
            leaf,
            part.content_type == "text/html",
            complete,
            &mut display,
        )? {
            return Ok(output.finish(true));
        }
    }
    Ok(output.finish(false))
}

/// Sample an already validated raw fetch from attachment filtering. The MIME
/// parser supplies exact leaf boundaries, so the samples need no EOF probes.
pub(in crate::mail) fn from_raw(
    raw: &[u8],
    page_remaining: &mut usize,
) -> Result<Preview, ReadFault> {
    if *page_remaining == 0 {
        return Ok(Preview::BudgetSkipped);
    }
    let message = mime::parse(raw).map_err(ReadFault::local)?;
    let mut selected = Vec::new();
    if selection::select(&message, &mut selected, true)
        .map_err(|_| ReadFault::local(MailError::backend()))?
    {
        return Err(ReadFault::local(html::unsupported()));
    }
    let mut budget = SampleBudget {
        message: MESSAGE_ENCODED_BYTES,
        page: page_remaining,
    };
    let mut output = PreviewText::default();
    let mut display = DisplayContext::new(HtmlDisplay::Readable);
    for part in selected {
        let encoded = part.encoded_body();
        if encoded.is_empty() {
            continue;
        }
        let sample = budget.available(encoded.len());
        if sample == 0 {
            return Ok(output.finish(true));
        }
        let encoding = part
            .headers
            .get_first_value("Content-Transfer-Encoding")
            .map_or_else(|| "7bit".into(), |value| value.trim().to_ascii_lowercase());
        let mut decoder = SampleDecoder::new(&part.ctype.charset, &encoding, sample)?;
        budget.claim(sample);
        decoder.feed(&encoded[..sample])?;
        let complete = sample == encoded.len();
        let leaf = decoder.finish(complete)?;
        if output.append_leaf(
            leaf,
            part.ctype.mimetype == "text/html",
            complete,
            &mut display,
        )? {
            return Ok(output.finish(true));
        }
    }
    Ok(output.finish(false))
}

#[cfg(test)]
mod tests;
