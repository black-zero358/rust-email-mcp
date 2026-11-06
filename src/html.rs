//! Bounded HTML-to-text extraction. Tokenize only: no DOM, CSS, layout, script
//! execution, resource loading or link expansion. The small input cap also
//! bounds the tokenizer's temporary tag/attribute allocation before callbacks.
use crate::error::{LimitKind, MailError, Result};
use html5ever::tokenizer::{
    BufferQueue, Token, TokenSink, TokenSinkResult, Tokenizer, states::RawKind,
};
use std::cell::RefCell;

mod readable;
pub(crate) use readable::{DisplayBody, DisplayContext, HtmlDisplay};

/// Join rendered inline leaves without fusing their contents across MIME boundaries.
fn append_body_leaf(out: &mut String, leaf: &str, remaining: &mut usize) -> Result<()> {
    if !out.is_empty()
        && !leaf.is_empty()
        && !out.ends_with(['\r', '\n'])
        && !leaf.starts_with(['\r', '\n'])
    {
        *remaining = remaining
            .checked_sub(1)
            .ok_or_else(|| MailError::limit_kind(LimitKind::DecodedBodyBytes))?;
        out.push('\n');
    }
    out.push_str(leaf);
    Ok(())
}

#[derive(Default)]
pub(crate) struct RenderWork {
    tokens: usize,
}

pub(crate) fn project_body_leaf(
    out: &mut String,
    leaf: &str,
    html: bool,
    reserve: usize,
    remaining: &mut usize,
    context: &mut DisplayContext,
    work: &mut RenderWork,
) -> Result<()> {
    let rendered = if html {
        std::borrow::Cow::Owned(render_selected(leaf, context, work)?)
    } else {
        std::borrow::Cow::Borrowed(leaf)
    };
    // Readable HTML accounts for generated indentation and link text separately.
    if rendered.len() > reserve && !(html && context.format() == HtmlDisplay::Readable) {
        return Err(MailError::limit_kind(LimitKind::DecodedBodyBytes));
    }
    append_body_leaf(out, &rendered, remaining)
}

pub(crate) const MAX_INPUT: usize = 1024 * 1024;
const MAX_TOKENS: usize = 32768;
const MAX_OUTPUT: usize = 1024 * 1024;
// Attribute boundaries can also use quoted-value adjacency or slash recovery
// without whitespace. Count all potential separators across the entire input,
// before tokenizer allocation, instead of trying to duplicate its lexer.
const MAX_MARKUP_SEPARATORS: usize = 65536;

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
    readable: Option<readable::Readable>,
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
        if let Some(readable) = &mut self.readable {
            readable.text(value);
            self.limited |= readable.limited();
            return;
        }
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
                    if let Some(readable) = &mut text.readable {
                        readable.tag(&tag);
                        text.limited |= readable.limited();
                    } else if matches!(
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
                    if text.readable.is_none()
                        && tag.kind == StartTag
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
            Token::ParseError(_) => {
                text.parse_error = true;
                if let Some(readable) = &mut text.readable {
                    readable.malformed();
                }
            }
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

pub(crate) fn render_selected(
    html: &str,
    context: &mut DisplayContext,
    work: &mut RenderWork,
) -> Result<String> {
    context.html_part();
    if context.format() == HtmlDisplay::Legacy {
        return tokenize(html, None, true, work).map(|(text, _)| text);
    }
    render_readable(html, context, true, work)
}

pub(crate) fn render_selected_prefix(
    html: &str,
    context: &mut DisplayContext,
    work: &mut RenderWork,
) -> Result<String> {
    context.html_part();
    render_readable(html, context, false, work)
}

fn render_readable(
    html: &str,
    context: &mut DisplayContext,
    complete: bool,
    work: &mut RenderWork,
) -> Result<String> {
    let (text, next) = tokenize(html, Some(std::mem::take(context)), complete, work)?;
    *context = next.ok_or_else(MailError::backend)?;
    Ok(text)
}

fn tokenize(
    html: &str,
    context: Option<DisplayContext>,
    complete: bool,
    work: &mut RenderWork,
) -> Result<(String, Option<DisplayContext>)> {
    check_input(html)?;
    let input = BufferQueue::default();
    input.push_back(html.into());
    let mut sink = Sink::default();
    sink.0.get_mut().tokens = work.tokens;
    if let Some(context) = context {
        sink.0.get_mut().readable = Some(readable::Readable::new(context));
    }
    let tokenizer = Tokenizer::new(sink, Default::default());
    let _ = tokenizer.feed(&input);
    if !input.is_empty() {
        let text = tokenizer.sink.0.borrow();
        work.tokens = text.tokens;
        return Err(if text.limited {
            MailError::limit()
        } else {
            unsupported()
        });
    }
    // At a sampling boundary the tokenizer may be holding a partial token.
    // Keep only output emitted from the available prefix.
    if complete {
        tokenizer.end();
    }
    let mut text = tokenizer.sink.0.into_inner();
    work.tokens = text.tokens;
    if text.limited {
        return Err(MailError::limit());
    }
    if text.unsupported {
        return Err(unsupported());
    }
    if let Some(readable) = text.readable {
        return readable.finish(text.parse_error, html, complete);
    }
    while text.out.ends_with('\n') {
        text.out.pop();
    }
    if text.out.is_empty() && text.parse_error && !html.trim().is_empty() {
        return Err(unsupported());
    }
    Ok((text.out, None))
}

pub(crate) fn unsupported() -> MailError {
    MailError {
        code: "UNSUPPORTED_BODY",
        message: "The original body cannot be rendered completely as plain text.",
        retryable: false,
        details: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to_text(html: &str) -> Result<String> {
        render_selected(
            html,
            &mut DisplayContext::default(),
            &mut RenderWork::default(),
        )
    }

    #[test]
    fn attributes_are_bounded_before_tokenization_without_whitespace() {
        for attributes in [
            (0..70000).map(|i| format!("a{i}=''")).collect::<String>(),
            (0..70000).map(|i| format!("/a{i}")).collect::<String>(),
        ] {
            let html = format!("<p {attributes}>visible</p>");
            assert!(html.len() < MAX_INPUT);
            assert_eq!(check_input(&html).unwrap_err().code, "LIMIT_EXCEEDED");
            assert_eq!(to_text(&html).unwrap_err().code, "LIMIT_EXCEEDED");
        }
        assert_eq!(to_text("<p a=''b=''>ordinary</p>").unwrap(), "ordinary");
    }
}
