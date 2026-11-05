//! Optional read-display policy. All state, addresses and output stay bounded;
//! this module performs no I/O and does not participate in MIME selection.
use super::{MAX_OUTPUT, unsupported};
use crate::error::{MailError, Result};
use html5ever::tokenizer::{Tag, TagKind};
use serde::Serialize;
use std::collections::HashSet;

const MAX_NESTING: usize = 16;
const MAX_LINK_BYTES: usize = 2048;
const MAX_LINKS: usize = 256;
const REASONS: [&str; 8] = [
    "link_scheme",
    "link_relative",
    "link_control",
    "link_length",
    "link_limit",
    "nesting_limit",
    "image_source",
    "malformed_structure",
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum HtmlDisplay {
    #[default]
    Legacy,
    Readable,
}

#[derive(Debug, Serialize)]
pub(crate) struct DisplayOmission {
    pub reason: &'static str,
    pub count: u32,
}

#[derive(Debug, Serialize)]
pub(crate) struct HtmlDisplayMetadata {
    pub format_version: &'static str,
    pub html_parts: u32,
    pub omitted: bool,
    pub omissions: Vec<DisplayOmission>,
}

#[derive(Debug)]
pub(crate) struct DisplayBody {
    pub text: String,
    pub metadata: HtmlDisplayMetadata,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(html: &str) -> DisplayBody {
        let mut context = DisplayContext::new(HtmlDisplay::Readable);
        let text = super::super::render_selected(html, &mut context).unwrap();
        DisplayBody {
            text,
            metadata: context.into_metadata(),
        }
    }
    fn count(body: &DisplayBody, reason: &str) -> u32 {
        body.metadata
            .omissions
            .iter()
            .find(|item| item.reason == reason)
            .map_or(0, |item| item.count)
    }

    #[test]
    fn readable_links_decode_allowlist_deduplicate_and_keep_empty_anchors() {
        let body = render(
            "<p><a href='https&#58;//example.test/?a=1&amp;b=2'>会议</a> <a href='https://example.test/?a=1&amp;b=2'>会议</a></p><a href='https://same.test/'>https://same.test/</a><br><a href='mailto:user@example.test'>邮件</a><br><a href='HTTPS://empty.test/'></a>",
        );
        assert_eq!(
            body.text,
            "会议 (https://example.test/?a=1&b=2) 会议\nhttps://same.test/\n邮件 (mailto:user@example.test)\nHTTPS://empty.test/"
        );
        assert_eq!(body.metadata.format_version, "readable-v1");
        assert_eq!(body.metadata.html_parts, 1);
        assert!(!body.metadata.omitted);
        assert_eq!(
            render("<blockquote><a href='https://same.test/'>https://same.test/</a></blockquote>")
                .text,
            "> https://same.test/"
        );
        let malformed_tail = "<a href='https://empty.test/'></a><b";
        assert_eq!(
            super::super::to_text(malformed_tail).unwrap_err().code,
            "UNSUPPORTED_BODY"
        );
        let body = render(malformed_tail);
        assert_eq!(body.text, "https://empty.test/");
        assert!(count(&body, "malformed_structure") >= 1);
    }

    #[test]
    fn readable_link_failures_only_report_fixed_reasons() {
        let body = render(
            "<a href='JaVaScRiPt:secret()'>脚本</a> <a href='data:text/plain,secret'>数据</a> <a href='file:///secret'>文件</a> <a href='../secret'>相对</a> <a href='https://exa&#10;mple.test/secret'>控制</a> <a href='https://example.test/&#x202e;secret'>方向</a>",
        );
        assert_eq!(body.text, "脚本 数据 文件 相对 控制 方向");
        assert_eq!(count(&body, "link_scheme"), 3);
        assert_eq!(count(&body, "link_relative"), 1);
        assert_eq!(count(&body, "link_control"), 2);
        assert!(
            !serde_json::to_string(&body.metadata)
                .unwrap()
                .contains("secret")
        );
        assert_eq!(
            render("<a href='//example.test/path'>relative</a>").text,
            "relative"
        );
    }

    #[test]
    fn readable_link_lengths_are_decoded_utf8_bytes_and_count_is_bounded() {
        let prefix = "https://example.test/";
        let exact = format!("{prefix}{}", "x".repeat(MAX_LINK_BYTES - prefix.len()));
        assert!(
            render(&format!("<a href='{exact}'>exact</a>"))
                .text
                .contains(&exact)
        );
        let body = render(&format!(
            "<a href='{exact}x'>long</a><a href='{prefix}{}'>中文</a>",
            "中".repeat(700)
        ));
        assert_eq!(body.text, "long中文");
        assert_eq!(count(&body, "link_length"), 2);
        let html = (0..257)
            .map(|i| format!("<a href='https://example.test/{i}'>item{i}</a><br>"))
            .collect::<String>();
        let body = render(&html);
        assert_eq!(body.text.matches(" (https://").count(), 256);
        assert_eq!(count(&body, "link_limit"), 1);
    }

    #[test]
    fn readable_lists_quotes_tables_pre_code_and_alt_have_stable_boundaries() {
        assert_eq!(
            render("<ul><li>A<ol start='3'><li>B</li><li>C</li></ol></li><li>D</li></ul>").text,
            "- A\n  3. B\n  4. C\n- D"
        );
        assert_eq!(
            render(
                "<blockquote><p>outer</p><blockquote>inner<br>again</blockquote>tail</blockquote>"
            )
            .text,
            "> outer\n> > inner\n> > again\n> tail"
        );
        assert_eq!(render("<table><tr><th>Name</th><th>Qty</th></tr><tr><td><p>中</p><p>row</p></td><td>2</td></tr></table>").text, "Name | Qty\n中 row | 2");
        assert_eq!(
            render("<p>intro</p><pre>  a\tb\n    c\n\n</pre>").text,
            "intro\n  a\tb\n    c\n\n"
        );
        assert_eq!(
            render("before <code>a  b\n c</code> after").text,
            "before a  b\n c after"
        );
        assert_eq!(
            render("<blockquote><pre>  one\n\ttwo</pre></blockquote>").text,
            ">   one\n> \ttwo"
        );
        assert_eq!(
            render(
                "<table><tr><td><pre> one\n  two</pre></td><td><code>a  b</code></td></tr></table>"
            )
            .text,
            "one two | a b"
        );
        let body = render(
            "<img src='https://example.test/pixel' alt='图 &amp; 文'><img src='cid:logo' alt=' 标志'>",
        );
        assert_eq!(body.text, "图 & 文 标志");
        assert_eq!(count(&body, "image_source"), 2);
    }

    #[test]
    fn readable_depth_malformed_unicode_foreign_and_existing_limits() {
        let html = format!("{}<li>深层😀</li>{}", "<ul>".repeat(17), "</ul>".repeat(17));
        let body = render(&html);
        assert_eq!(count(&body, "nesting_limit"), 1);
        assert_eq!(body.text, format!("{}- 深层😀", "  ".repeat(15)));
        let body =
            render("<p a=''b=''>中 &euro; &#x1f600;</p><a href='https://example.test/'>unfinished");
        assert!(
            body.text
                .contains("中 € 😀\nunfinished (https://example.test/)")
        );
        assert!(count(&body, "malformed_structure") >= 1);
        for foreign in ["<svg><![CDATA[secret]]></svg>", "<math>secret</math>"] {
            let mut context = DisplayContext::new(HtmlDisplay::Readable);
            assert_eq!(
                super::super::render_selected(&format!("visible{foreign}"), &mut context)
                    .unwrap_err()
                    .code,
                "UNSUPPORTED_BODY"
            );
        }
        let mut context = DisplayContext::new(HtmlDisplay::Readable);
        assert_eq!(
            super::super::render_selected(&"<b></b>".repeat(32768), &mut context)
                .unwrap_err()
                .code,
            "LIMIT_EXCEEDED"
        );
        let mut context = DisplayContext::new(HtmlDisplay::Readable);
        let amplified = format!(
            "{}<pre>{}</pre>{}",
            "<blockquote>".repeat(16),
            "x\n".repeat(32000),
            "</blockquote>".repeat(16)
        );
        assert_eq!(
            super::super::render_selected(&amplified, &mut context)
                .unwrap_err()
                .code,
            "LIMIT_EXCEEDED"
        );
    }
}

#[derive(Default)]
pub(crate) struct DisplayContext {
    format: HtmlDisplay,
    html_parts: u32,
    tokens: usize,
    output_bytes: usize,
    links: usize,
    // At most 256 pairs, with <=2048-byte labels and addresses. Longer labels
    // stay visible but are excluded from the optional repeated-pair shortcut.
    seen_links: HashSet<(String, String)>,
    omissions: [u32; 8],
}

impl DisplayContext {
    pub fn new(format: HtmlDisplay) -> Self {
        Self {
            format,
            ..Self::default()
        }
    }
    pub fn format(&self) -> HtmlDisplay {
        self.format
    }
    pub fn html_part(&mut self) {
        self.html_parts = self.html_parts.saturating_add(1);
    }
    pub fn tokens(&self) -> usize {
        self.tokens
    }
    fn omit(&mut self, reason: usize) {
        self.omissions[reason] = self.omissions[reason].saturating_add(1);
    }
    pub fn into_metadata(self) -> HtmlDisplayMetadata {
        HtmlDisplayMetadata {
            format_version: match self.format {
                HtmlDisplay::Legacy => "legacy-v1",
                HtmlDisplay::Readable => "readable-v1",
            },
            html_parts: self.html_parts,
            omitted: self.omissions.iter().any(|count| *count != 0),
            omissions: REASONS
                .into_iter()
                .zip(self.omissions)
                .filter(|(_, count)| *count != 0)
                .map(|(reason, count)| DisplayOmission { reason, count })
                .collect(),
        }
    }
}

struct List {
    ordered: bool,
    next: u32,
}
#[derive(Default)]
struct Table {
    cell: bool,
    columns: usize,
}
struct Anchor {
    url: Option<String>,
    label: String,
    label_limited: bool,
    pending_space: bool,
}
impl Anchor {
    fn text(&mut self, value: &str) {
        for c in value.chars() {
            if self.label_limited {
                return;
            }
            if c.is_whitespace() {
                self.pending_space = !self.label.is_empty();
            } else if self.label.len() + usize::from(self.pending_space) + c.len_utf8()
                > MAX_LINK_BYTES
            {
                self.label_limited = true;
            } else {
                if self.pending_space {
                    self.label.push(' ');
                }
                self.pending_space = false;
                self.label.push(c);
            }
        }
    }
}

pub(super) struct Readable {
    context: DisplayContext,
    out: String,
    limited: bool,
    pending_space: bool,
    lists: Vec<List>,
    list_overflow: usize,
    quotes: usize,
    quote_overflow: usize,
    tables: Vec<Table>,
    table_overflow: usize,
    pre: usize,
    code: usize,
    anchor: Option<Anchor>,
    preserve_tail: bool,
}

impl Readable {
    pub fn new(context: DisplayContext) -> Self {
        Self {
            context,
            out: String::new(),
            limited: false,
            pending_space: false,
            lists: Vec::new(),
            list_overflow: 0,
            quotes: 0,
            quote_overflow: 0,
            tables: Vec::new(),
            table_overflow: 0,
            pre: 0,
            code: 0,
            anchor: None,
            preserve_tail: false,
        }
    }
    pub fn limited(&self) -> bool {
        self.limited
    }
    pub fn malformed(&mut self) {
        self.context.omit(7);
    }
    fn push(&mut self, value: char) {
        if self.context.output_bytes + value.len_utf8() > MAX_OUTPUT {
            self.limited = true;
        } else if !self.limited {
            self.out.push(value);
            self.context.output_bytes += value.len_utf8();
        }
    }
    fn write(&mut self, value: &str) {
        for c in value.chars() {
            self.push(c);
            if c != '\n' {
                self.preserve_tail = false;
            }
        }
    }
    fn prefix(&mut self) {
        if self.out.is_empty() || self.out.ends_with('\n') {
            for _ in 0..self.quotes {
                self.write("> ");
            }
        }
    }
    fn in_cell(&self) -> bool {
        self.tables.last().is_some_and(|table| table.cell)
    }
    fn newline(&mut self) {
        if self.in_cell() {
            self.pending_space = true;
        } else {
            self.linebreak();
        }
    }
    fn linebreak(&mut self) {
        self.pending_space = false;
        if !self.out.is_empty() && !self.out.ends_with('\n') {
            self.push('\n');
        }
    }
    pub fn text(&mut self, value: &str) {
        if let Some(anchor) = &mut self.anchor {
            anchor.text(value);
        }
        for c in value.chars() {
            if self.in_cell() && c.is_whitespace() {
                self.pending_space = true;
            } else if self.pre != 0 || self.code != 0 {
                if c != '\n' {
                    self.prefix();
                }
                if self.pending_space && !self.out.is_empty() && !self.out.ends_with([' ', '\n']) {
                    self.push(' ');
                }
                self.pending_space = false;
                self.push(c);
                self.preserve_tail = c == '\n';
            } else if c.is_whitespace() {
                self.pending_space = true;
            } else {
                self.prefix();
                if self.pending_space && !self.out.is_empty() && !self.out.ends_with([' ', '\n']) {
                    self.push(' ');
                }
                self.pending_space = false;
                self.push(c);
                self.preserve_tail = false;
            }
        }
    }
    fn attr<'a>(tag: &'a Tag, name: &str) -> Option<&'a str> {
        tag.attrs
            .iter()
            .find(|attr| &*attr.name.local == name)
            .map(|attr| attr.value.as_ref())
    }
    fn link_url(&mut self, href: &str) -> Option<String> {
        self.context.links += 1;
        let failure = if self.context.links > MAX_LINKS {
            Some(4)
        } else if href.len() > MAX_LINK_BYTES {
            Some(3)
        } else if href.chars().any(|c| {
            c.is_control()
                || c.is_whitespace()
                || matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        }) {
            Some(2)
        } else if let Some((scheme, _)) = href.split_once(':') {
            if ["http", "https", "mailto"]
                .iter()
                .any(|allowed| scheme.eq_ignore_ascii_case(allowed))
            {
                match reqwest::Url::parse(href) {
                    Ok(url)
                        if (matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
                            || (url.scheme() == "mailto" && !url.path().is_empty()) =>
                    {
                        None
                    }
                    _ => Some(0),
                }
            } else {
                Some(0)
            }
        } else {
            Some(1)
        };
        if let Some(reason) = failure {
            self.context.omit(reason);
            None
        } else {
            Some(href.to_owned())
        }
    }
    fn close_anchor(&mut self) {
        if let Some(Anchor {
            url: Some(url),
            label,
            label_limited,
            ..
        }) = self.anchor.take()
        {
            if (!label_limited && label == url)
                || (!label_limited && !self.context.seen_links.insert((label.clone(), url.clone())))
            {
                return;
            }
            if label.is_empty() {
                self.text(&url);
            } else {
                self.pending_space = false;
                self.write(" (");
                self.write(&url);
                self.push(')');
            }
        }
    }
    pub fn tag(&mut self, tag: &Tag) {
        let start = tag.kind == TagKind::StartTag;
        let name = tag.name.as_ref();
        match (name, start) {
            ("a", true) => {
                if self.anchor.is_some() {
                    self.malformed();
                    self.close_anchor();
                }
                let url = Self::attr(tag, "href").and_then(|href| self.link_url(href));
                self.anchor = Some(Anchor {
                    url,
                    label: String::new(),
                    label_limited: false,
                    pending_space: false,
                });
            }
            ("a", false) => self.close_anchor(),
            ("ul" | "ol", true) => {
                self.newline();
                if self.lists.len() == MAX_NESTING {
                    self.list_overflow += 1;
                    self.context.omit(5);
                } else {
                    self.lists.push(List {
                        ordered: name == "ol",
                        next: Self::attr(tag, "start")
                            .and_then(|value| value.parse::<u32>().ok())
                            .unwrap_or(1),
                    });
                }
            }
            ("ul" | "ol", false) => {
                self.newline();
                if self.list_overflow != 0 {
                    self.list_overflow -= 1;
                } else {
                    match self.lists.pop() {
                        Some(list) if list.ordered == (name == "ol") => {}
                        _ => self.malformed(),
                    }
                }
            }
            ("li", true) => {
                self.newline();
                self.prefix();
                for _ in 1..self.lists.len() {
                    self.write("  ");
                }
                if let Some(list) = self.lists.last_mut() {
                    let marker = if list.ordered {
                        let value = format!("{}. ", list.next);
                        list.next = list.next.saturating_add(1);
                        value
                    } else {
                        "- ".into()
                    };
                    self.write(&marker);
                } else {
                    self.malformed();
                    self.write("- ");
                }
            }
            ("li", false) => self.newline(),
            ("blockquote", true) => {
                self.newline();
                if self.quotes == MAX_NESTING {
                    self.quote_overflow += 1;
                    self.context.omit(5);
                } else {
                    self.quotes += 1;
                }
            }
            ("blockquote", false) => {
                self.newline();
                if self.quote_overflow != 0 {
                    self.quote_overflow -= 1;
                } else if self.quotes != 0 {
                    self.quotes -= 1;
                } else {
                    self.malformed();
                }
            }
            ("table", true) => {
                self.newline();
                if self.tables.len() == MAX_NESTING {
                    self.table_overflow += 1;
                    self.context.omit(5);
                } else {
                    self.tables.push(Table::default());
                }
            }
            ("table", false) => {
                if self.table_overflow != 0 {
                    self.table_overflow -= 1;
                } else if self.tables.pop().is_none() {
                    self.malformed();
                }
                self.newline();
            }
            ("tr", _) => {
                if let Some(table) = self.tables.last_mut() {
                    table.cell = false;
                    table.columns = 0;
                } else {
                    self.malformed();
                }
                self.linebreak();
            }
            ("td" | "th", true) => {
                let separator = if let Some(table) = self.tables.last_mut() {
                    let separator = table.columns != 0;
                    table.cell = true;
                    table.columns += 1;
                    separator
                } else {
                    self.malformed();
                    false
                };
                self.pending_space = false;
                if separator {
                    self.write(" | ");
                }
            }
            ("td" | "th", false) => {
                if let Some(table) = self.tables.last_mut() {
                    table.cell = false;
                }
                self.pending_space = false;
            }
            ("pre", true) => {
                self.newline();
                self.pre += 1;
            }
            ("pre", false) => {
                self.pre = self.pre.saturating_sub(1);
                self.newline();
            }
            ("code", true) => self.code += 1,
            ("code", false) => self.code = self.code.saturating_sub(1),
            ("img", true) => {
                if Self::attr(tag, "src").is_some_and(|src| !src.is_empty()) {
                    self.context.omit(6);
                }
                if let Some(alt) = Self::attr(tag, "alt") {
                    self.text(alt);
                }
            }
            (
                "br" | "p" | "div" | "hr" | "section" | "article" | "header" | "footer" | "h1"
                | "h2" | "h3" | "h4" | "h5" | "h6",
                _,
            ) => self.newline(),
            _ => {}
        }
    }
    pub fn finish(
        mut self,
        tokens: usize,
        parse_error: bool,
        html: &str,
        complete: bool,
    ) -> Result<(String, Option<DisplayContext>)> {
        if complete {
            if self.anchor.is_some() {
                self.malformed();
                self.close_anchor();
            }
            if !self.lists.is_empty()
                || self.quotes != 0
                || !self.tables.is_empty()
                || self.pre != 0
                || self.code != 0
            {
                self.malformed();
            }
        }
        if self.limited {
            return Err(MailError::limit());
        }
        if self.out.is_empty() && parse_error && !html.trim().is_empty() {
            return Err(unsupported());
        }
        while !self.preserve_tail && self.out.ends_with('\n') {
            self.out.pop();
        }
        self.context.tokens = tokens;
        Ok((self.out, Some(self.context)))
    }
}
