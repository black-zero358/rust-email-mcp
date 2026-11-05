//! Bounded Message-ID parsing and comparable identity evidence for conversation state.
use mailparse::MailHeader;
use sha2::{Digest, Sha256};

const MAX_ID_BYTES: usize = 998;
pub(super) const MAX_IDS_PER_MESSAGE: usize = 1024;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(super) struct MessageId(Box<str>);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct HeaderLimit;

impl MessageId {
    pub(super) fn parse(value: &[u8]) -> Result<Option<Self>, HeaderLimit> {
        let Ok(value) = std::str::from_utf8(value) else {
            return Ok(None);
        };
        let Some(value) = cfws(value) else {
            return Ok(None);
        };
        let Some((id, rest)) = parse_id(value)? else {
            return Ok(None);
        };
        Ok(cfws(rest).is_some_and(str::is_empty).then_some(id))
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }

    /// An ASCII domain atom occurs in both modern and obsolete forms. If no
    /// such anchor exists, an empty HEADER search covers the entire field.
    pub(super) fn search_anchor(&self) -> &str {
        if self.0.ends_with("]>") {
            return "";
        }
        self.0
            .strip_suffix('>')
            .and_then(|id| id.rsplit_once('@'))
            .and_then(|(_, right)| {
                right
                    .split('.')
                    .filter(|word| word.is_ascii())
                    .max_by_key(|word| word.len())
            })
            .unwrap_or("")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ContentFingerprint {
    pub method: u16,
    pub digest: [u8; 32],
}

impl ContentFingerprint {
    /// Hash the complete root MIME body, from after the header delimiter to
    /// EOF, including encoded payloads and embedded MIME headers and boundaries.
    pub(super) fn raw_mime_body(body: &[u8]) -> Self {
        Self {
            method: 1,
            digest: Sha256::digest(body).into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Evidence {
    pub headers: [u8; 32],
    pub content: Option<ContentFingerprint>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Comparison {
    Match,
    Conflict,
    Unverified,
}

impl Evidence {
    pub(super) fn compare(self, other: Self) -> Comparison {
        if self.headers != other.headers {
            return Comparison::Conflict;
        }
        match (self.content, other.content) {
            (Some(a), Some(b)) if a.method == b.method => {
                if a.digest == b.digest {
                    Comparison::Match
                } else {
                    Comparison::Conflict
                }
            }
            _ => Comparison::Unverified,
        }
    }
}

pub(super) struct ThreadFacts {
    pub id: Option<MessageId>,
    pub references: Vec<MessageId>,
    pub in_reply_to: Vec<MessageId>,
    pub evidence: Evidence,
}

impl ThreadFacts {
    /// Input is a complete, already bounded header block. Structured IDs are
    /// taken from the wire value, without RFC 2047 encoded-word decoding.
    pub(super) fn parse(headers: &[MailHeader<'_>]) -> Result<Self, HeaderLimit> {
        let mut id_fields = headers.iter().filter(|header| {
            header
                .get_key_raw()
                .trim_ascii_end()
                .eq_ignore_ascii_case(b"message-id")
        });
        let id = match (id_fields.next(), id_fields.next()) {
            (Some(header), None) => MessageId::parse(header.get_value_raw())?,
            _ => None,
        };
        let mut references = Vec::new();
        let mut in_reply_to = Vec::new();
        let mut count = usize::from(id.is_some());
        for header in headers {
            let field = header.get_key_raw().trim_ascii_end();
            if field.eq_ignore_ascii_case(b"references") {
                parse_references(header.get_value_raw(), &mut references, &mut count)?;
            } else if field.eq_ignore_ascii_case(b"in-reply-to") {
                parse_references(header.get_value_raw(), &mut in_reply_to, &mut count)?;
            }
        }
        let mut hash = Sha256::new();
        hash.update(b"conversation-critical-headers-v2\0");
        for field in [
            "from",
            "sender",
            "reply-to",
            "to",
            "cc",
            "bcc",
            "date",
            "subject",
            "references",
            "in-reply-to",
            "mime-version",
            "content-type",
            "content-disposition",
            "content-transfer-encoding",
            "content-id",
            "content-description",
            "content-language",
            "content-location",
        ] {
            hash.update(field);
            hash.update([0]);
            for header in headers.iter().filter(|header| {
                header
                    .get_key_raw()
                    .trim_ascii_end()
                    .eq_ignore_ascii_case(field.as_bytes())
            }) {
                hash.update([1]);
                // Display decoding can replace invalid octets, interpret one
                // byte sequence under multiple encodings, or collapse folds.
                // Identity evidence preserves the bounded wire value instead.
                hash_field(&mut hash, header.get_value_raw());
            }
            hash.update([0]);
        }
        Ok(Self {
            id,
            references,
            in_reply_to,
            evidence: Evidence {
                headers: hash.finalize().into(),
                content: None,
            },
        })
    }
}

fn hash_field(hash: &mut Sha256, value: &[u8]) {
    hash.update((value.len() as u64).to_be_bytes());
    hash.update(value);
}

fn parse_references(
    value: &[u8],
    out: &mut Vec<MessageId>,
    count: &mut usize,
) -> Result<(), HeaderLimit> {
    let Ok(mut value) = std::str::from_utf8(value) else {
        return Ok(());
    };
    while let Some(remaining) = cfws(value).filter(|rest| !rest.is_empty()) {
        value = remaining;
        if value.starts_with('<')
            && let Some((id, rest)) = parse_id(value)?
        {
            if *count >= MAX_IDS_PER_MESSAGE {
                return Err(HeaderLimit);
            }
            *count += 1;
            if !out.contains(&id) {
                out.push(id);
            }
            value = rest;
        } else if value.starts_with('"') {
            // Obsolete reference fields may interleave phrases and IDs.
            let Some((_, rest)) = quoted(value, '"', '"')? else {
                break;
            };
            value = rest;
        } else if value.starts_with('<') {
            let Some((_, rest)) = value.split_once('>') else {
                break;
            };
            value = rest;
        } else {
            value = &value[value
                .char_indices()
                .nth(1)
                .map_or(value.len(), |(index, _)| index)..];
        }
    }
    Ok(())
}

fn parse_id(value: &str) -> Result<Option<(MessageId, &str)>, HeaderLimit> {
    let Some(value) = value.strip_prefix('<').and_then(cfws) else {
        return Ok(None);
    };
    let Some((local, rest)) = dot_words(value, true)? else {
        return Ok(None);
    };
    let Some(right) = rest.strip_prefix('@').and_then(cfws) else {
        return Ok(None);
    };
    let (domain, rest, literal) = if right.starts_with('[') {
        let Some((domain, rest)) = quoted(right, '[', ']')? else {
            return Ok(None);
        };
        let Some(rest) = cfws(rest) else {
            return Ok(None);
        };
        (domain, rest, true)
    } else {
        let Some((domain, rest)) = dot_words(right, false)? else {
            return Ok(None);
        };
        (domain, rest, false)
    };
    let Some(rest) = rest.strip_prefix('>') else {
        return Ok(None);
    };
    let mut canonical = String::from("<");
    if local
        .split('.')
        .all(|atom| !atom.is_empty() && atom.chars().all(atext))
    {
        push_str(&mut canonical, &local)?;
    } else {
        append_quoted(&mut canonical, &local, '"', '"')?;
    }
    push_str(&mut canonical, "@")?;
    if literal {
        append_quoted(&mut canonical, &domain, '[', ']')?;
    } else {
        push_str(&mut canonical, &domain)?;
    }
    push_str(&mut canonical, ">")?;
    Ok(Some((MessageId(canonical.into_boxed_str()), rest)))
}

fn dot_words(mut value: &str, local: bool) -> Result<Option<(String, &str)>, HeaderLimit> {
    let mut words = String::new();
    loop {
        if local && value.starts_with('"') {
            let Some((word, rest)) = quoted(value, '"', '"')? else {
                return Ok(None);
            };
            push_str(&mut words, &word)?;
            value = rest;
        } else {
            let end = value.find(|ch| !atext(ch)).unwrap_or(value.len());
            if end == 0 {
                return Ok(None);
            }
            push_str(&mut words, &value[..end])?;
            value = &value[end..];
        }
        let Some(rest) = cfws(value) else {
            return Ok(None);
        };
        let Some(next) = rest.strip_prefix('.') else {
            return Ok(Some((words, rest)));
        };
        push_str(&mut words, ".")?;
        let Some(next) = cfws(next) else {
            return Ok(None);
        };
        value = next;
    }
}

fn atext(ch: char) -> bool {
    ch.is_ascii_alphanumeric()
        || "!#$%&'*+-/=?^_`{|}~".contains(ch)
        || !ch.is_ascii() && !ch.is_control()
}

fn quoted(value: &str, open: char, close: char) -> Result<Option<(String, &str)>, HeaderLimit> {
    let Some(value) = value.strip_prefix(open) else {
        return Ok(None);
    };
    let mut text = String::new();
    let mut chars = value.char_indices();
    while let Some((offset, ch)) = chars.next() {
        match ch {
            ch if ch == close => return Ok(Some((text, &value[offset + ch.len_utf8()..]))),
            '\\' => {
                let Some((_, escaped)) = chars.next() else {
                    return Ok(None);
                };
                if !quoted_character(escaped) {
                    return Ok(None);
                }
                push_char(&mut text, escaped)?;
            }
            '\r' if value[offset..].starts_with("\r\n ")
                || value[offset..].starts_with("\r\n\t") =>
            {
                chars.next();
            }
            ch if quoted_character(ch) && (open == close || ch != open) => {
                push_char(&mut text, ch)?
            }
            _ => return Ok(None),
        }
    }
    Ok(None)
}

fn quoted_character(ch: char) -> bool {
    ch == '\t' || !ch.is_control()
}

fn append_quoted(out: &mut String, text: &str, open: char, close: char) -> Result<(), HeaderLimit> {
    push_char(out, open)?;
    for ch in text.chars() {
        if ch == '\\' || ch == close || ch == open {
            push_char(out, '\\')?;
        }
        push_char(out, ch)?;
    }
    push_char(out, close)
}

fn push_str(out: &mut String, value: &str) -> Result<(), HeaderLimit> {
    if out.len() + value.len() > MAX_ID_BYTES {
        return Err(HeaderLimit);
    }
    out.push_str(value);
    Ok(())
}

fn push_char(out: &mut String, value: char) -> Result<(), HeaderLimit> {
    if out.len() + value.len_utf8() > MAX_ID_BYTES {
        return Err(HeaderLimit);
    }
    out.push(value);
    Ok(())
}

fn cfws(mut value: &str) -> Option<&str> {
    loop {
        value = value.trim_start_matches([' ', '\t']);
        if let Some(rest) = value
            .strip_prefix("\r\n")
            .filter(|rest| rest.starts_with([' ', '\t']))
        {
            value = rest;
            continue;
        }
        if !value.starts_with('(') {
            return Some(value);
        }
        let mut depth = 0usize;
        let mut escaped = false;
        let (end, _) = value.char_indices().find(|&(_, ch)| {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '(' {
                depth += 1;
            } else if ch == ')' {
                depth -= 1;
            }
            depth == 0
        })?;
        value = &value[end + 1..];
    }
}

#[cfg(test)]
#[path = "tests/identity.rs"]
mod tests;
