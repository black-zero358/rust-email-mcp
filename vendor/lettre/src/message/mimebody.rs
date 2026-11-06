use std::{io::Write, iter::repeat_with};

use mime::Mime;

use crate::message::{
    EmailFormat, IntoBody,
    header::{self, ContentTransferEncoding, ContentType, Header, Headers},
};

/// MIME part variants
#[derive(Debug, Clone)]
pub(super) enum Part {
    /// Single part with content
    Single(SinglePart),

    /// Multiple parts of content
    Multi(MultiPart),
}

impl Part {
    #[cfg(feature = "dkim")]
    pub(super) fn format_body(&self, out: &mut Vec<u8>) {
        match self {
            Part::Single(part) => part.format_body(out),
            Part::Multi(part) => part.format_body(out),
        }
    }
}

impl EmailFormat for Part {
    fn format(&self, out: &mut Vec<u8>) {
        match self {
            Part::Single(part) => part.format(out),
            Part::Multi(part) => part.format(out),
        }
    }
}

/// Creates builder for single part
#[derive(Debug, Clone)]
pub struct SinglePartBuilder {
    headers: Headers,
}

impl SinglePartBuilder {
    /// Creates a default singlepart builder
    pub fn new() -> Self {
        Self {
            headers: Headers::new(),
        }
    }

    /// Set the header to singlepart
    pub fn header<H: Header>(mut self, header: H) -> Self {
        self.headers.set(header);
        self
    }

    /// Set the Content-Type header of the singlepart
    pub fn content_type(mut self, content_type: ContentType) -> Self {
        self.headers.set(content_type);
        self
    }

    /// Build singlepart using body
    pub fn body<T: IntoBody>(mut self, body: T) -> SinglePart {
        let maybe_encoding = self.headers.get::<ContentTransferEncoding>();
        let body = body.into_body(maybe_encoding);

        self.headers.set(body.encoding());

        SinglePart {
            headers: self.headers,
            body: body.into_vec(),
        }
    }
}

impl Default for SinglePartBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Single part
///
/// # Example
///
/// ```
/// use lettre::message::{SinglePart, header};
///
/// # use std::error::Error;
/// # fn main() -> Result<(), Box<dyn Error>> {
/// let part = SinglePart::builder()
///     .header(header::ContentType::TEXT_PLAIN)
///     .body(String::from("Текст письма в уникоде"));
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct SinglePart {
    headers: Headers,
    body: Vec<u8>,
}

impl SinglePart {
    /// Creates a builder for singlepart
    #[inline]
    pub fn builder() -> SinglePartBuilder {
        SinglePartBuilder::new()
    }

    /// Directly create a `SinglePart` from a plain UTF-8 content
    pub fn plain<T: IntoBody>(body: T) -> Self {
        Self::builder()
            .header(header::ContentType::TEXT_PLAIN)
            .body(body)
    }

    /// Directly create a `SinglePart` from a UTF-8 HTML content
    pub fn html<T: IntoBody>(body: T) -> Self {
        Self::builder()
            .header(header::ContentType::TEXT_HTML)
            .body(body)
    }

    /// Get the headers from singlepart
    #[inline]
    pub fn headers(&self) -> &Headers {
        &self.headers
    }

    /// Get the encoded body
    #[inline]
    pub fn raw_body(&self) -> &[u8] {
        &self.body
    }

    /// Get message content formatted for sending
    pub fn formatted(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.format(&mut out);
        out
    }

    /// Format only the signlepart body
    fn format_body(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.body);
        out.extend_from_slice(b"\r\n");
    }
}

impl EmailFormat for SinglePart {
    fn format(&self, out: &mut Vec<u8>) {
        write!(out, "{}", self.headers)
            .expect("A Write implementation panicked while formatting headers");
        out.extend_from_slice(b"\r\n");
        self.format_body(out);
    }
}

/// The kind of multipart
#[derive(Debug, Clone)]
pub enum MultiPartKind {
    /// Mixed kind to combine unrelated content parts
    ///
    /// For example, this kind can be used to mix an email message and attachments.
    Mixed,

    /// Alternative kind to join several variants of same email contents.
    ///
    /// That kind is recommended to use for joining plain (text) and rich (HTML) messages into a single email message.
    Alternative,

    /// Related kind to mix content and related resources.
    ///
    /// For example, you can include images in HTML content using that.
    Related,

    /// Encrypted kind for encrypted messages
    Encrypted { protocol: String },

    /// Signed kind for signed messages
    Signed { protocol: String, micalg: String },
}

/// Create a random MIME boundary.
/// (Not cryptographically random)
fn make_boundary() -> String {
    repeat_with(fastrand::alphanumeric).take(40).collect()
}

impl MultiPartKind {
    pub(crate) fn to_mime<S: Into<String>>(&self, boundary: Option<S>) -> Mime {
        let boundary = boundary.map_or_else(make_boundary, Into::into);

        format!(
            "multipart/{}; boundary=\"{}\"{}",
            match self {
                Self::Mixed => "mixed",
                Self::Alternative => "alternative",
                Self::Related => "related",
                Self::Encrypted { .. } => "encrypted",
                Self::Signed { .. } => "signed",
            },
            boundary,
            match self {
                Self::Encrypted { protocol } => format!("; protocol=\"{protocol}\""),
                Self::Signed { protocol, micalg } =>
                    format!("; protocol=\"{protocol}\"; micalg=\"{micalg}\""),
                _ => String::new(),
            }
        )
        .parse()
        .unwrap()
    }

    fn from_mime(m: &Mime) -> Option<Self> {
        match m.subtype().as_ref() {
            "mixed" => Some(Self::Mixed),
            "alternative" => Some(Self::Alternative),
            "related" => Some(Self::Related),
            "signed" => m.get_param("protocol").and_then(|p| {
                m.get_param("micalg").map(|micalg| Self::Signed {
                    protocol: p.as_str().to_owned(),
                    micalg: micalg.as_str().to_owned(),
                })
            }),
            "encrypted" => m.get_param("protocol").map(|p| Self::Encrypted {
                protocol: p.as_str().to_owned(),
            }),
            _ => None,
        }
    }
}

/// Multipart builder
#[derive(Debug, Clone)]
pub struct MultiPartBuilder {
    headers: Headers,
}

impl MultiPartBuilder {
    /// Creates default multipart builder
    pub fn new() -> Self {
        Self {
            headers: Headers::new(),
        }
    }

    /// Set a header
    pub fn header<H: Header>(mut self, header: H) -> Self {
        self.headers.set(header);
        self
    }

    /// Set `Content-Type` header using [`MultiPartKind`]
    pub fn kind(self, kind: MultiPartKind) -> Self {
        self.header(ContentType::from_mime(kind.to_mime::<String>(None)))
    }

    /// Set custom boundary
    pub fn boundary<S: Into<String>>(self, boundary: S) -> Self {
        let kind = {
            let content_type = self.headers.get::<ContentType>().unwrap();
            MultiPartKind::from_mime(content_type.as_ref()).unwrap()
        };
        let mime = kind.to_mime(Some(boundary));
        self.header(ContentType::from_mime(mime))
    }

    /// Creates multipart without parts
    pub fn build(self) -> MultiPart {
        MultiPart {
            headers: self.headers,
            parts: Vec::new(),
        }
    }

    /// Creates multipart using singlepart
    pub fn singlepart(self, part: SinglePart) -> MultiPart {
        self.build().singlepart(part)
    }

    /// Creates multipart using multipart
    pub fn multipart(self, part: MultiPart) -> MultiPart {
        self.build().multipart(part)
    }
}

impl Default for MultiPartBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Multipart variant with parts
#[derive(Debug, Clone)]
pub struct MultiPart {
    headers: Headers,
    parts: Vec<Part>,
}

impl MultiPart {
    /// Creates multipart builder
    pub fn builder() -> MultiPartBuilder {
        MultiPartBuilder::new()
    }

    /// Creates mixed multipart builder
    ///
    /// Shortcut for `MultiPart::builder().kind(MultiPartKind::Mixed)`
    pub fn mixed() -> MultiPartBuilder {
        MultiPart::builder().kind(MultiPartKind::Mixed)
    }

    /// Creates alternative multipart builder
    ///
    /// Shortcut for `MultiPart::builder().kind(MultiPartKind::Alternative)`
    pub fn alternative() -> MultiPartBuilder {
        MultiPart::builder().kind(MultiPartKind::Alternative)
    }

    /// Creates related multipart builder
    ///
    /// Shortcut for `MultiPart::builder().kind(MultiPartKind::Related)`
    pub fn related() -> MultiPartBuilder {
        MultiPart::builder().kind(MultiPartKind::Related)
    }

    /// Creates encrypted multipart builder
    ///
    /// Shortcut for `MultiPart::builder().kind(MultiPartKind::Encrypted{ protocol })`
    pub fn encrypted(protocol: String) -> MultiPartBuilder {
        MultiPart::builder().kind(MultiPartKind::Encrypted { protocol })
    }

    /// Creates signed multipart builder
    ///
    /// Shortcut for `MultiPart::builder().kind(MultiPartKind::Signed{ protocol, micalg })`
    pub fn signed(protocol: String, micalg: String) -> MultiPartBuilder {
        MultiPart::builder().kind(MultiPartKind::Signed { protocol, micalg })
    }

    /// Alias for HTML and plain text versions of an email
    pub fn alternative_plain_html<T: IntoBody, V: IntoBody>(plain: T, html: V) -> Self {
        Self::alternative()
            .singlepart(SinglePart::plain(plain))
            .singlepart(SinglePart::html(html))
    }

    /// Add single part to multipart
    pub fn singlepart(mut self, part: SinglePart) -> Self {
        self.parts.push(Part::Single(part));
        self
    }

    /// Add multi part to multipart
    pub fn multipart(mut self, part: MultiPart) -> Self {
        self.parts.push(Part::Multi(part));
        self
    }

    /// Get the boundary of multipart contents
    pub fn boundary(&self) -> String {
        let content_type = self.headers.get::<ContentType>().unwrap();
        content_type
            .as_ref()
            .get_param("boundary")
            .unwrap()
            .as_str()
            .into()
    }

    /// Get the headers from the multipart
    pub fn headers(&self) -> &Headers {
        &self.headers
    }

    /// Get a mutable reference to the headers
    pub fn headers_mut(&mut self) -> &mut Headers {
        &mut self.headers
    }

    /// Get message content formatted for SMTP
    pub fn formatted(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.format(&mut out);
        out
    }

    /// Format only the multipart body
    fn format_body(&self, out: &mut Vec<u8>) {
        let boundary = self.boundary();

        for part in &self.parts {
            out.extend_from_slice(b"--");
            out.extend_from_slice(boundary.as_bytes());
            out.extend_from_slice(b"\r\n");
            part.format(out);
        }

        out.extend_from_slice(b"--");
        out.extend_from_slice(boundary.as_bytes());
        out.extend_from_slice(b"--\r\n");
    }
}

impl EmailFormat for MultiPart {
    fn format(&self, out: &mut Vec<u8>) {
        write!(out, "{}", self.headers)
            .expect("A Write implementation panicked while formatting headers");
        out.extend_from_slice(b"\r\n");
        self.format_body(out);
    }
}
