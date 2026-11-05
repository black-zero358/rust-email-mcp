//! Header-only classification. Values are used locally and never serialized.
use mailparse::MailHeader;
use reqwest::Url;
use serde::Serialize;
use std::borrow::Cow;

const RULE_VERSION: &str = "headers-v1";
const MAX_URI_BYTES: usize = 2048;

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Coverage {
    Complete,
    Partial,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Category {
    Newsletter,
    Automated,
    Unclassified,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Field {
    ListId,
    ListUnsubscribe,
    ListUnsubscribePost,
    AutoSubmitted,
    Precedence,
}

#[derive(Serialize)]
struct Signal {
    field: Field,
    category: Category,
}

#[derive(Serialize)]
pub(super) struct Classification {
    classification: Vec<Category>,
    classification_signals: Vec<Signal>,
    classification_rule_version: &'static str,
    classification_status: Coverage,
    unsubscribe_available: bool,
}

/// For a clipped header literal, keep only fields whose final line is known.
/// Even a complete CRLF may still be followed by an unfetched continuation.
pub(super) fn complete_header_prefix(raw: &[u8]) -> &[u8] {
    let mut offset = 0;
    let mut complete_end = 0;
    for line in raw.split_inclusive(|byte| *byte == b'\n') {
        if line == b"\r\n" || line == b"\n" {
            return &raw[..offset];
        }
        if !line.starts_with(b" ") && !line.starts_with(b"\t") {
            complete_end = offset;
        }
        offset += line.len();
    }
    &raw[..complete_end]
}

/// Accepts already bounded and parsed headers. For partial coverage, the caller
/// passes only the fields from `complete_header_prefix`.
pub(super) fn classify(headers: &[MailHeader<'_>], coverage: Coverage) -> Classification {
    let mut list_id = false;
    let mut unsubscribe = false;
    let mut unsubscribe_available = false;
    let mut unsubscribe_post = false;
    let mut auto_submitted = false;
    let mut precedence_list = false;
    let mut precedence_automated = false;
    for header in headers {
        let field = header.get_key_raw();
        let value = header.get_value_raw().trim_ascii();
        if field.eq_ignore_ascii_case(b"list-id") {
            list_id |= valid_list_id(value);
        } else if field.eq_ignore_ascii_case(b"list-unsubscribe") {
            let (has_uri, supported) = unsubscribe_uris(value);
            unsubscribe |= has_uri;
            unsubscribe_available |= supported;
        } else if field.eq_ignore_ascii_case(b"list-unsubscribe-post") {
            // RFC 8058 defines the token. It is only a list signal here.
            unsubscribe_post |= value.eq_ignore_ascii_case(b"list-unsubscribe=one-click");
        } else if field.eq_ignore_ascii_case(b"auto-submitted") {
            // RFC 3834 permits extension keywords and ignorable parameters.
            auto_submitted |=
                keyword(value, true).is_some_and(|token| !token.eq_ignore_ascii_case(b"no"));
        } else if field.eq_ignore_ascii_case(b"precedence") {
            // Precedence is a legacy convention with a fixed local allowlist.
            if let Some(token) = keyword(value, false) {
                precedence_list |= token.eq_ignore_ascii_case(b"list");
                precedence_automated |= [b"bulk".as_slice(), b"junk", b"auto_reply"]
                    .iter()
                    .any(|known| token.eq_ignore_ascii_case(known));
            }
        }
    }

    use Category::{Automated, Newsletter, Unclassified};
    use Field::{AutoSubmitted, ListId, ListUnsubscribe, ListUnsubscribePost, Precedence};
    // OR duplicate evidence and emit each field/category at most once in rule order.
    let classification_signals = [
        (list_id, ListId, Newsletter),
        (unsubscribe, ListUnsubscribe, Newsletter),
        (unsubscribe_post, ListUnsubscribePost, Newsletter),
        (auto_submitted, AutoSubmitted, Automated),
        (precedence_list, Precedence, Newsletter),
        (precedence_automated, Precedence, Automated),
    ]
    .into_iter()
    .filter(|(matched, _, _)| *matched)
    .map(|(_, field, category)| Signal { field, category })
    .collect::<Vec<_>>();
    let mut classification = [Newsletter, Automated]
        .into_iter()
        .filter(|category| {
            classification_signals
                .iter()
                .any(|signal| signal.category == *category)
        })
        .collect::<Vec<_>>();
    if classification.is_empty() {
        classification.push(Unclassified);
    }
    Classification {
        classification,
        classification_signals,
        classification_rule_version: RULE_VERSION,
        classification_status: coverage,
        unsubscribe_available,
    }
}

fn valid_list_id(value: &[u8]) -> bool {
    // RFC 2919: ignore the optional phrase and whitespace within the brackets.
    let Some(end) = value.strip_suffix(b">") else {
        return false;
    };
    let Some(start) = end.iter().rposition(|byte| *byte == b'<') else {
        return false;
    };
    let mut size = 0;
    let mut dot = false;
    let mut atom = false;
    for byte in end[start + 1..]
        .iter()
        .copied()
        .filter(|byte| !byte.is_ascii_whitespace())
    {
        size += 1;
        if size > 255 {
            return false;
        }
        match byte {
            b'.' if atom => {
                dot = true;
                atom = false;
            }
            b if b.is_ascii_alphanumeric() || b"!#$%&'*+-/=?^_`{|}~".contains(&b) => atom = true,
            _ => return false,
        }
    }
    atom && dot
}

fn keyword(value: &[u8], parameters_allowed: bool) -> Option<&[u8]> {
    let (word, tail) = mime_token(skip_cfws(value)?)?;
    let mut tail = skip_cfws(tail)?;
    while parameters_allowed && let Some(parameter) = tail.strip_prefix(b";") {
        let (_, tail_after_name) = mime_token(skip_cfws(parameter)?)?;
        let value = skip_cfws(skip_cfws(tail_after_name)?.strip_prefix(b"=")?)?;
        let tail_after_value = if let Some(quoted) = value.strip_prefix(b"\"") {
            let mut escaped = false;
            let end = quoted.iter().position(|byte| {
                if escaped {
                    escaped = false;
                    return false;
                }
                escaped = *byte == b'\\';
                *byte == b'"'
            })?;
            &quoted[end + 1..]
        } else {
            mime_token(value)?.1
        };
        tail = skip_cfws(tail_after_value)?;
    }
    tail.is_empty().then_some(word)
}

fn mime_token(value: &[u8]) -> Option<(&[u8], &[u8])> {
    let end = value
        .iter()
        .position(|byte| !(b'!'..=b'~').contains(byte) || b"()<>@,;:\\\"/[]?=".contains(byte))
        .unwrap_or(value.len());
    (end > 0).then(|| value.split_at(end))
}

/// Skip whitespace and balanced RFC 5322 comments, including quoted pairs.
fn skip_cfws(mut value: &[u8]) -> Option<&[u8]> {
    loop {
        value = value.trim_ascii_start();
        if !value.starts_with(b"(") {
            return Some(value);
        }
        let mut depth = 0;
        let mut escaped = false;
        let end = value.iter().position(|byte| {
            if escaped {
                escaped = false;
            } else {
                match byte {
                    b'\\' => escaped = true,
                    b'(' => depth += 1,
                    b')' => depth -= 1,
                    _ => {}
                }
            }
            depth == 0
        })?;
        value = &value[end + 1..];
    }
}

fn unsubscribe_uris(mut value: &[u8]) -> (bool, bool) {
    // RFC 2369 lists angle-bracketed alternatives. A malformed item ends the list.
    let mut has_uri = false;
    while let Some(item) = skip_cfws(value).and_then(|value| value.strip_prefix(b"<")) {
        let Some(end) = item.iter().position(|byte| *byte == b'>') else {
            break;
        };
        let Some(supported) = supported_unsubscribe_uri(&item[..end]) else {
            break;
        };
        has_uri = true;
        if supported {
            return (true, true);
        }
        let Some(next) = skip_cfws(&item[end + 1..]).and_then(|tail| tail.strip_prefix(b","))
        else {
            break;
        };
        value = next;
    }
    (has_uri, false)
}

/// `None` is invalid syntax. `Some(false)` allows a later alternative URI.
fn supported_unsubscribe_uri(mut bytes: &[u8]) -> Option<bool> {
    let mut normalized = Vec::new();
    while let Some((&byte, rest)) = bytes.split_first() {
        bytes = rest;
        match byte {
            b' ' | b'\t' => continue,
            b'\r' if rest.starts_with(b"\n ") || rest.starts_with(b"\n\t") => {
                bytes = &rest[1..];
                continue;
            }
            b'\n' if rest.starts_with(b" ") || rest.starts_with(b"\t") => continue,
            b'!'..=b'~' if !b"\"<>\\^`{|}".contains(&byte) => {}
            _ => return None,
        }
        if normalized.len() == MAX_URI_BYTES {
            return None;
        }
        normalized.push(byte);
    }
    let uri = std::str::from_utf8(&normalized).ok()?;
    for (index, _) in uri.match_indices('%') {
        let escaped = normalized.get(index + 1..index + 3)?;
        if !escaped.iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
    }
    let url = Url::parse(uri).ok()?;
    Some(match url.scheme() {
        "http" | "https" => {
            // URL parsing otherwise repairs missing slashes and empty userinfo.
            let authority = uri
                .split_once(':')?
                .1
                .strip_prefix("//")?
                .split(['/', '?', '#'])
                .next()?;
            if authority.is_empty() {
                return None;
            }
            url.has_host() && !authority.contains('@')
        }
        "mailto" => valid_mailto(&url).unwrap_or(false),
        _ => false,
    })
}

fn valid_mailto(url: &Url) -> Option<bool> {
    if !url.cannot_be_a_base() || url.fragment().is_some() {
        return Some(false);
    }
    let mut has_recipient = !url.path().is_empty();
    if has_recipient && !valid_recipients(url.path(), false) {
        return Some(false);
    }
    if let Some(query) = url.query() {
        for pair in query.split('&') {
            let (key, value) = pair.split_once('=')?;
            let qchar =
                |byte: u8| byte.is_ascii_alphanumeric() || b"-._~%!$'()*+,;:@".contains(&byte);
            if !key.bytes().chain(value.bytes()).all(qchar) {
                return Some(false);
            }
            let key = decode_percent(key, |_| true)?;
            if key.is_empty()
                || !key
                    .bytes()
                    .all(|byte| (b'!'..=b'~').contains(&byte) && byte != b':')
            {
                return Some(false);
            }
            if key.eq_ignore_ascii_case("to") && !value.is_empty() {
                if !valid_recipients(value, true) {
                    return Some(false);
                }
                has_recipient = true;
            }
        }
    }
    Some(has_recipient)
}

fn valid_recipients(recipients: &str, query: bool) -> bool {
    recipients.split(',').all(|recipient| {
        // Separate URI delimiters before decoding either address component.
        let literal_at = recipient.rfind('@').map(|at| (at, 1));
        let query_at = if query {
            recipient.rfind("%40").map(|at| (at, 3))
        } else {
            None
        };
        literal_at.max(query_at).is_some_and(|(at, len)| {
            valid_local_part(&recipient[..at]) && valid_domain(&recipient[at + len..])
        })
    })
}

fn valid_domain(domain: &str) -> bool {
    // RFC 6068 permits percent-encoded UTF-8 in DNS names. Also accept encoded
    // brackets for the address literal form, which is checked separately.
    let Some(domain) = decode_percent(domain, |byte| !byte.is_ascii() || b"[]".contains(&byte))
    else {
        return false;
    };
    if let Some(literal) = domain
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
    {
        if let Some((version, addr)) = literal.split_once(':')
            && version.eq_ignore_ascii_case("ipv6")
        {
            return addr.parse::<std::net::Ipv6Addr>().is_ok();
        }
        return literal.parse::<std::net::Ipv4Addr>().is_ok();
    }
    if domain
        .bytes()
        .any(|byte| byte.is_ascii() && !byte.is_ascii_alphanumeric() && !b"-.".contains(&byte))
    {
        return false;
    }
    if domain.is_ascii()
        && !domain.split('.').any(|label| {
            label
                .get(..4)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("xn--"))
        })
    {
        return valid_dns_name(&domain);
    }
    // Use the existing URL parser for IDNA conversion without DNS or I/O.
    Url::parse(&format!("https://{domain}"))
        .is_ok_and(|url| url.domain().is_some_and(valid_dns_name))
}

fn valid_dns_name(domain: &str) -> bool {
    domain.len() <= 253
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

fn valid_local_part(local: &str) -> bool {
    // RFC 6068 requires these characters to be escaped inside an addr-spec.
    if local.contains(['/', '&', ';', '=', '[', ']']) {
        return false;
    }
    let Some(local) = decode_percent(local, |_| true) else {
        return false;
    };
    if local.chars().any(char::is_control) {
        return false;
    }
    if let Some(quoted) = local
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    {
        let mut escaped = false;
        for character in quoted.chars() {
            if escaped {
                escaped = false;
            } else if character == '"' {
                return false;
            } else {
                escaped = character == '\\';
            }
        }
        return !escaped;
    }
    local.split('.').all(|atom| {
        !atom.is_empty()
            && atom.chars().all(|character| {
                character.is_ascii_alphanumeric()
                    || "!#$%&'*+-/=?^_`{|}~".contains(character)
                    || !character.is_ascii() && !character.is_whitespace()
            })
    })
}

fn decode_percent(value: &str, allowed: impl Fn(u8) -> bool) -> Option<Cow<'_, str>> {
    let mut sections = value.split('%');
    let first = sections.next()?;
    let mut escapes = sections.peekable();
    if escapes.peek().is_none() {
        return Some(Cow::Borrowed(value));
    }
    let mut decoded = Vec::with_capacity(value.len());
    decoded.extend_from_slice(first.as_bytes());
    for escape in escapes {
        let (hex, rest) = escape.split_at_checked(2)?;
        let byte = u8::from_str_radix(hex, 16).ok()?;
        if !allowed(byte) {
            return None;
        }
        decoded.push(byte);
        decoded.extend_from_slice(rest.as_bytes());
    }
    String::from_utf8(decoded).ok().map(Cow::Owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn read(raw: &[u8], coverage: Coverage) -> Value {
        let (headers, _) = mailparse::parse_headers(raw).unwrap();
        serde_json::to_value(classify(&headers, coverage)).unwrap()
    }

    #[test]
    fn header_categories_are_ordered_deduplicated_and_hide_source_values() {
        let classified = read(
            b"Precedence: bulk\r\nPrecedence: LIST\r\n\
              Auto-Submitted: (automation) X-Private-Extension; x-name=secret\r\n\
              List-Unsubscribe-Post: LIST-UNSUBSCRIBE=ONE-CLICK\r\n\
              List-Unsubscribe: (reason) <ftp://private.test/data>,\r\n\t<https://private.test/secret>\r\n\
              LIST-ID: Private list <secret.list.\r\n test>\r\n\
              List-ID: <second.secret.test>\r\n\r\n",
            Coverage::Complete,
        );
        assert_eq!(
            classified,
            json!({
                "classification": ["newsletter", "automated"],
                "classification_signals": [
                    {"field":"list-id", "category":"newsletter"},
                    {"field":"list-unsubscribe", "category":"newsletter"},
                    {"field":"list-unsubscribe-post", "category":"newsletter"},
                    {"field":"auto-submitted", "category":"automated"},
                    {"field":"precedence", "category":"newsletter"},
                    {"field":"precedence", "category":"automated"}
                ],
                "classification_rule_version":"headers-v1",
                "classification_status":"complete",
                "unsubscribe_available":true
            })
        );
    }

    #[test]
    fn empty_malformed_and_no_headers_leave_message_unclassified() {
        for header in [
            b"Auto-Submitted: (why) No (more); x-note=\"private reason\"\r\n".as_slice(),
            b"Auto-Submitted: \r\n",
            b"Auto-Submitted: two words\r\n",
            b"Auto-Submitted: auto-generated; malformed\r\n",
            b"Auto-Submitted: auto-generated; x-reason=\"unclosed\r\n",
            b"Auto-Submitted: =?utf-8?Q?auto-generated?=\r\n",
            b"Precedence: bulk-mail\r\n",
            b"Precedence: List, Bulk\r\n",
            b"List-Id: placeholder text\r\n",
            b"List-Id: <missing-namespace>\r\n",
            b"List-Id: <not..a.list.test>\r\n",
            b"List-Unsubscribe: broken, <https://private.test/path>\r\n",
            b"List-Unsubscribe: \r\n",
            b"List-Unsubscribe-Post: List-Unsubscribe=other\r\n",
            b"Subject: List-Unsubscribe: <https://private.test/path>\r\n",
        ] {
            let result = read(header, Coverage::Complete);
            assert_eq!(
                result["classification"],
                json!(["unclassified"]),
                "{header:?}"
            );
            assert_eq!(result["classification_signals"], json!([]));
            assert_eq!(result["unsubscribe_available"], false);
        }
    }

    #[test]
    fn automation_uses_exact_legacy_precedence_and_non_no_auto_submitted_keywords() {
        for header in [
            "Precedence: bulk",
            "Precedence: JuNk",
            "Precedence: auto_reply",
            "Auto-Submitted: auto-generated",
            "Auto-Submitted: auto-replied (automatic reply)",
            "Auto-Submitted: auto-generated; x-reason=\"private \\\"parameter; value\"; x-flag=yes",
            "Auto-Submitted: (outer (inner \\) text)) x-sample; x-value=opaque",
        ] {
            let result = read(header.as_bytes(), Coverage::Complete);
            assert_eq!(result["classification"], json!(["automated"]), "{header}");
        }
        let list = read(b"Precedence: LIST (bulk)", Coverage::Complete);
        assert_eq!(list["classification"], json!(["newsletter"]));
    }

    #[test]
    fn unsubscribe_availability_only_accepts_bounded_supported_uris() {
        for (uri, valid) in [
            ("<https://example.test/path?recipient=opaque>", true),
            ("<HTTP://example.test/path>", true),
            ("<mailto:user+tag@example.test?subject=unsubscribe>", true),
            ("<mailto:?TO=user+tag%40example.test>", true),
            ("<mailto:user@[192.0.2.1]>", true),
            (
                "<mailto:%22other%40local%22@example.test,two@example.test>",
                true,
            ),
            (
                "<ftp://example.test/file> (unsupported), <https://ex\r\n ample.test/>",
                true,
            ),
            ("<https://user:secret@example.test/path>", false),
            ("<https://user@example.test/path>", false),
            ("<https://@example.test/path>", false),
            ("<https:/example.test/path>", false),
            ("<https:///example.test/path>", false),
            ("<https://example.test/\u{0000}x>", false),
            ("<https://example.test/%x0>", false),
            ("<mailto:no-domain>", false),
            ("<mailto:@example.test>", false),
            ("<mailto:user@example.test/path>", false),
            ("<mailto:user%0A@example.test>", false),
            ("<mailto:?subject=unsubscribe>", false),
            ("<mailto://user@example.test>", false),
            ("<javascript:do-not-run()>", false),
            (
                "<ftp://example.test/file>, broken, <https://example.test/>",
                false,
            ),
        ] {
            let header = format!("List-Unsubscribe: {uri}\r\n");
            assert_eq!(
                read(header.as_bytes(), Coverage::Complete)["unsubscribe_available"],
                valid,
                "{uri}"
            );
        }
        let post_only = read(
            b"List-Unsubscribe-Post: List-Unsubscribe=One-Click",
            Coverage::Complete,
        );
        assert_eq!(post_only["classification"], json!(["newsletter"]));
        assert_eq!(post_only["unsubscribe_available"], false);

        let prefix = "https://example.test/";
        for (size, available) in [(MAX_URI_BYTES, true), (MAX_URI_BYTES + 1, false)] {
            let header = format!(
                "List-Unsubscribe: <{prefix}{}>",
                "x".repeat(size - prefix.len())
            );
            assert_eq!(
                read(header.as_bytes(), Coverage::Complete)["unsubscribe_available"],
                available
            );
        }
    }

    #[test]
    fn mailto_validates_decoded_address_components_after_splitting_uri_delimiters() {
        for (target, available) in [
            ("a%2Fb%26c%3Dd%25f@example.test", true),
            ("%22a%40b%2Cc%5C%22d%3B%20e%22@example.test", true),
            ("%22oh%5C%5Cno%22@example.test", true),
            ("%E4%B8%AD@b%C3%BCcher.example", true),
            ("user@xn--bcher-kva.example", true),
            ("user@%5BIPv6:2001:db8::1%5D", true),
            (
                "?t%6F=a%2Fb%26c%3Dd%40b%C3%BCcher.example&subject=unsubscribe",
                true,
            ),
            ("user@example.test?body=line%0D%0Anext", true),
            ("a/b@example.test", false),
            ("a&b@example.test", false),
            ("a=b@example.test", false),
            ("a;b@example.test", false),
            ("a%3Bb@example.test", false),
            ("?to=a=b@example.test", false),
            ("?to=a/b@example.test", false),
            ("user@example.test?cc=bob@example.test?body=hello", false),
            ("user@example.test?%00=x", false),
            ("user@example.test?%3A=x", false),
            ("user@example.test?=x", false),
            ("user@example.test?%C3%A9=x", false),
            ("user@%2F.example.test", false),
            ("user@%FF.example.test", false),
            ("user@%C3.example.test", false),
            ("user@%65xample.test", false),
            ("user@%252F.example.test", false),
            ("user@%EF%BC%BF.example.test", false),
            ("user@xn--.example.test", false),
            ("user@-broken.example.test", false),
            ("user%FF@example.test", false),
            ("a%40b@example.test", false),
            ("a%20b@example.test", false),
            ("a%2Cb@example.test", false),
            ("%22unbalanced%22quote%22@example.test", false),
            ("%22unfinished%5C%22@example.test", false),
            ("user@example.test,invalid@%2F.example.test", false),
        ] {
            let header = format!("List-Unsubscribe: <mailto:{target}>\r\n");
            assert_eq!(
                read(header.as_bytes(), Coverage::Complete)["unsubscribe_available"],
                available,
                "{target}"
            );
        }
        let alternatives = read(
            b"List-Unsubscribe: <mailto:user@%FF.example.test>, <https://example.test/unsubscribe>",
            Coverage::Complete,
        );
        assert_eq!(alternatives["unsubscribe_available"], true);
    }

    #[test]
    fn clipped_headers_drop_the_entire_unconfirmed_logical_field() {
        let list_id = b"List-Id: <verified.example.test>\r\n";
        for partial in [
            b"Auto-Submitted: auto-generated".as_slice(),
            b"Auto-Submitted: auto-generated\r\n",
            b"Auto-Submitted: auto-generated\r\n\t(private folded suffix)",
            b"List-Unsubscribe: <https://private.test/secret>\r\n\t",
        ] {
            let raw = [list_id.as_slice(), partial].concat();
            let safe = complete_header_prefix(&raw);
            assert_eq!(safe, list_id);
            let result = read(safe, Coverage::Partial);
            assert_eq!(result["classification"], json!(["newsletter"]));
            assert_eq!(result["classification_status"], "partial");
            assert_eq!(result["unsubscribe_available"], false);
        }
        assert!(complete_header_prefix(list_id).is_empty());
        assert_eq!(
            complete_header_prefix(&[list_id.as_slice(), b"\r\nbody"].concat()),
            list_id
        );
        assert_eq!(
            complete_header_prefix(&[list_id.as_slice(), b"N"].concat()),
            list_id
        );
    }
}
