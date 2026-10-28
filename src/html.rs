//! Bounded HTML-to-text extraction. Tokenize only: no DOM, CSS, layout, script
//! execution, resource loading or link expansion. The small input cap also
//! bounds the tokenizer's temporary tag/attribute allocation before callbacks.
use crate::error::{MailError, Result};
use html5ever::tokenizer::{
    BufferQueue, Token, TokenSink, TokenSinkResult, Tokenizer, states::RawKind,
};
use std::cell::RefCell;

const MAX_INPUT: usize = 128 * 1024;
const MAX_TOKENS: usize = 4096;
const MAX_OUTPUT: usize = 256 * 1024;
// Attribute boundaries can also use quoted-value adjacency or slash recovery
// without whitespace. Count all potential separators across the entire input,
// before tokenizer allocation, instead of trying to duplicate its lexer.
const MAX_MARKUP_SEPARATORS: usize = 8192;

#[derive(Default)]
struct Text {
    out: String,
    pending_space: bool,
    tokens: usize,
    limited: bool,
    in_head: bool,
    templates: usize,
    raw_ignore: Option<&'static str>,
    parse_error: bool,
    unsupported: bool,
}

impl Text {
    fn push(&mut self, c: char) {
        if self.out.len() + c.len_utf8() > MAX_OUTPUT {
            self.limited = true;
        } else if !self.limited {
            self.out.push(c);
        }
    }
    fn text(&mut self, value: &str) {
        for c in value.chars() {
            if c.is_whitespace() {
                self.pending_space = true;
            } else {
                if self.pending_space && !self.out.is_empty() && !self.out.ends_with('\n') {
                    self.push(' ');
                }
                self.pending_space = false;
                self.push(c);
            }
        }
    }
    fn newline(&mut self) {
        self.pending_space = false;
        if !self.out.is_empty() && !self.out.ends_with('\n') {
            self.push('\n');
        }
    }
}

#[derive(Default)]
struct Sink(RefCell<Text>);
impl TokenSink for Sink {
    type Handle = ();
    fn process_token(&self, token: Token, _: u64) -> TokenSinkResult<()> {
        let mut text = self.0.borrow_mut();
        if text.unsupported {
            return TokenSinkResult::Continue;
        }
        text.tokens += 1;
        if text.tokens > MAX_TOKENS || text.limited {
            text.limited = true;
            // Consume remaining bounded input without further tag objects.
            return if matches!(&token, Token::TagToken(_)) {
                TokenSinkResult::Plaintext
            } else {
                TokenSinkResult::Continue
            };
        }
        match token {
            Token::CharacterTokens(value) if text.raw_ignore.is_none() && text.templates == 0 => {
                if text.in_head && !value.trim().is_empty() {
                    text.in_head = false;
                }
                if !text.in_head {
                    text.text(&value);
                }
            }
            Token::NullCharacterToken
                if text.raw_ignore.is_none() && !text.in_head && text.templates == 0 =>
            {
                text.text("\u{fffd}")
            }
            Token::TagToken(tag) => {
                use html5ever::tokenizer::TagKind::{EndTag, StartTag};
                let name: &str = tag.name.as_ref();
                // A tokenizer-only sink has no foreign-namespace tree state.
                // Reject the whole body, even after visible text, rather than
                // discarding SVG/MathML CDATA as an HTML bogus comment.
                if tag.kind == StartTag && matches!(name, "svg" | "math") {
                    text.unsupported = true;
                    return TokenSinkResult::Plaintext;
                }
                if let Some(ignored) = text.raw_ignore {
                    if tag.kind == EndTag && name == ignored {
                        text.raw_ignore = None;
                    }
                    return TokenSinkResult::Continue;
                }
                if tag.kind == StartTag {
                    match name {
                        "script" => {
                            text.raw_ignore = Some("script");
                            return TokenSinkResult::RawData(RawKind::ScriptData);
                        }
                        "style" => {
                            text.raw_ignore = Some("style");
                            return TokenSinkResult::RawData(RawKind::Rawtext);
                        }
                        "iframe" => {
                            text.raw_ignore = Some("iframe");
                            return TokenSinkResult::RawData(RawKind::Rawtext);
                        }
                        "noframes" => {
                            text.raw_ignore = Some("noframes");
                            return TokenSinkResult::RawData(RawKind::Rawtext);
                        }
                        "noembed" => {
                            text.raw_ignore = Some("noembed");
                            return TokenSinkResult::RawData(RawKind::Rawtext);
                        }
                        "title" => {
                            text.raw_ignore = Some("title");
                            return TokenSinkResult::RawData(RawKind::Rcdata);
                        }
                        "head" => text.in_head = true,
                        "body" => text.in_head = false,
                        "template" => text.templates += 1,
                        _ => {
                            if text.in_head
                                && !matches!(
                                    name,
                                    "base" | "basefont" | "bgsound" | "link" | "meta" | "noframes"
                                )
                            {
                                text.in_head = false;
                            }
                        }
                    }
                } else {
                    match name {
                        "head" => text.in_head = false,
                        "template" => text.templates = text.templates.saturating_sub(1),
                        _ => {}
                    }
                }
                if !text.in_head && text.templates == 0 {
                    if matches!(
                        name,
                        "br" | "p"
                            | "div"
                            | "li"
                            | "ul"
                            | "ol"
                            | "tr"
                            | "td"
                            | "th"
                            | "table"
                            | "hr"
                            | "blockquote"
                            | "section"
                            | "article"
                            | "header"
                            | "footer"
                            | "h1"
                            | "h2"
                            | "h3"
                            | "h4"
                            | "h5"
                            | "h6"
                            | "pre"
                    ) {
                        text.newline();
                    }
                    if tag.kind == StartTag
                        && name == "img"
                        && let Some(alt) = tag.attrs.iter().find(|a| &*a.name.local == "alt")
                    {
                        text.text(&alt.value);
                    }
                }
                if tag.kind == StartTag && name == "textarea" {
                    return TokenSinkResult::RawData(RawKind::Rcdata);
                }
                if tag.kind == StartTag && name == "xmp" {
                    return TokenSinkResult::RawData(RawKind::Rawtext);
                }
                if tag.kind == StartTag && name == "plaintext" {
                    return TokenSinkResult::Plaintext;
                }
            }
            Token::ParseError(_) => text.parse_error = true,
            _ => {}
        }
        TokenSinkResult::Continue
    }
}

fn check_input(html: &str) -> Result<()> {
    if html.len() > MAX_INPUT
        || html
            .bytes()
            .filter(|b| b.is_ascii_whitespace() || matches!(b, b'\'' | b'"' | b'/' | b'='))
            .count()
            > MAX_MARKUP_SEPARATORS
    {
        return Err(MailError::limit());
    }
    Ok(())
}

pub(crate) fn to_text(html: &str) -> Result<String> {
    check_input(html)?;
    let input = BufferQueue::default();
    input.push_back(html.into());
    let tokenizer = Tokenizer::new(Sink::default(), Default::default());
    let _ = tokenizer.feed(&input);
    if !input.is_empty() {
        return Err(unsupported());
    }
    tokenizer.end();
    let mut text = tokenizer.sink.0.into_inner();
    if text.unsupported {
        return Err(unsupported());
    }
    if text.limited {
        return Err(MailError::limit());
    }
    while text.out.ends_with('\n') {
        text.out.pop();
    }
    if text.out.is_empty() && text.parse_error && !html.trim().is_empty() {
        return Err(unsupported());
    }
    Ok(text.out)
}

pub(crate) fn unsupported() -> MailError {
    MailError {
        code: "UNSUPPORTED_BODY",
        message: "The original body cannot be rendered completely as plain text.",
        retryable: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn html_text_preserves_entities_unicode_blocks_and_alt() {
        let html = "<html><head><title>metadata</title><style>.x { color: red }</style><script>if (a < b) { document.write('SCRIPT_ONLY'); }</script></head><body><h1>Invoice &amp; Friday</h1><p>中 &euro; &#x1F600;</p><p><a href='https://example.test/'>visible link</a><img src='https://example.test/image' alt=' image description'></p></body></html>";
        assert_eq!(
            to_text(html).unwrap(),
            "Invoice & Friday\n中 € 😀\nvisible link image description"
        );
    }
    #[test]
    fn raw_text_templates_and_comments_do_not_leak() {
        assert_eq!(to_text("<p>Visible</p><!-- COMMENT_ONLY --><template>HIDDEN<template>NESTED</template></template><script>const x = '<p>SCRIPT_ONLY</p>';</script><style>STYLE_ONLY</style><iframe src='https://example.test'>FRAME_ONLY</iframe><p>Body</p>").unwrap(), "Visible\nBody");
        assert_eq!(
            to_text("<textarea>&lt;literal&gt; &amp; text</textarea>").unwrap(),
            "<literal> & text"
        );
        assert_eq!(
            to_text("<body><p title='quoted > delimiter'>text</p></body>").unwrap(),
            "text"
        );
        assert_eq!(
            to_text("<head><title>metadata</title><p>Visible with implicit head end</p>").unwrap(),
            "Visible with implicit head end"
        );
        assert_eq!(to_text("a\0b").unwrap(), "a\u{fffd}b");
    }
    #[test]
    fn limits_fail_closed_then_recover() {
        assert_eq!(
            to_text(&"x".repeat(MAX_INPUT + 1)).unwrap_err().code,
            "LIMIT_EXCEEDED"
        );
        assert_eq!(
            to_text(&" ".repeat(MAX_MARKUP_SEPARATORS + 1))
                .unwrap_err()
                .code,
            "LIMIT_EXCEEDED"
        );
        assert_eq!(
            to_text(&"<b></b>".repeat(MAX_TOKENS)).unwrap_err().code,
            "LIMIT_EXCEEDED"
        );
        let mut text = Text {
            out: "x".repeat(MAX_OUTPUT - 1),
            ..Default::default()
        };
        text.push('中');
        assert!(text.limited);
        assert_eq!(text.out.len(), MAX_OUTPUT - 1);
        assert_eq!(to_text("<p>ordinary</p>").unwrap(), "ordinary");
        assert_eq!(to_text("<html><body></body></html>").unwrap(), "");
    }
    #[test]
    fn attributes_are_bounded_before_tokenization_without_whitespace() {
        for attributes in [
            (0..10000).map(|i| format!("a{i}=''")).collect::<String>(),
            (0..10000).map(|i| format!("/a{i}")).collect::<String>(),
        ] {
            let html = format!("<p {attributes}>visible</p>");
            assert!(html.len() < MAX_INPUT);
            assert_eq!(check_input(&html).unwrap_err().code, "LIMIT_EXCEEDED");
            assert_eq!(to_text(&html).unwrap_err().code, "LIMIT_EXCEEDED");
        }
        assert_eq!(to_text("<p a=''b=''>ordinary</p>").unwrap(), "ordinary");
    }
}
