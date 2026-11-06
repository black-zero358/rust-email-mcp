//! Headers widely used in email messages

use std::{
    borrow::Cow,
    error::Error,
    fmt::{self, Display, Formatter, Write},
    ops::Deref,
};

use email_encoding::headers::writer::EmailWriter;

pub use self::{
    content::*,
    content_disposition::ContentDisposition,
    content_type::{ContentType, ContentTypeErr},
    date::Date,
    mailbox::*,
    special::*,
    textual::*,
};
use crate::BoxError;

mod content;
mod content_disposition;
mod content_type;
mod date;
mod mailbox;
mod special;
mod textual;

/// Represents an email header
///
/// Email header as defined in [RFC5322](https://datatracker.ietf.org/doc/html/rfc5322) and extensions.
pub trait Header: Clone {
    fn name() -> HeaderName;

    fn parse(s: &str) -> Result<Self, BoxError>;

    fn display(&self) -> HeaderValue;
}

/// A set of email headers
#[derive(Debug, Clone, Default)]
pub struct Headers {
    headers: Vec<HeaderValue>,
}

impl Headers {
    /// Create an empty `Headers`
    ///
    /// This function does not allocate.
    #[inline]
    pub const fn new() -> Self {
        Self {
            headers: Vec::new(),
        }
    }

    /// Create an empty `Headers` with a pre-allocated capacity
    ///
    /// Pre-allocates a capacity of at least `capacity`.
    #[inline]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            headers: Vec::with_capacity(capacity),
        }
    }

    /// Returns a copy of a `Header` present in `Headers`
    ///
    /// Returns `None` if `Header` isn't present in `Headers`.
    pub fn get<H: Header>(&self) -> Option<H> {
        self.get_raw(&H::name())
            .and_then(|raw_value| H::parse(raw_value).ok())
    }

    /// Sets `Header` into `Headers`, overriding `Header` if it
    /// was already present in `Headers`
    pub fn set<H: Header>(&mut self, header: H) {
        self.insert_raw(header.display());
    }

    /// Remove `Header` from `Headers`, returning it
    ///
    /// Returns `None` if `Header` isn't in `Headers`.
    pub fn remove<H: Header>(&mut self) -> Option<H> {
        self.remove_raw(&H::name())
            .and_then(|value| H::parse(&value.raw_value).ok())
    }

    /// Clears `Headers`, removing all headers from it
    ///
    /// Any pre-allocated capacity is left untouched.
    #[inline]
    pub fn clear(&mut self) {
        self.headers.clear();
    }

    /// Returns a reference to the raw value of header `name`
    ///
    /// Returns `None` if `name` isn't present in `Headers`.
    pub fn get_raw(&self, name: &str) -> Option<&str> {
        self.find_header(name).map(|value| value.raw_value.as_str())
    }

    /// Inserts a raw header into `Headers`, overriding `value` if it
    /// was already present in `Headers`.
    pub fn insert_raw(&mut self, value: HeaderValue) {
        match self.find_header_mut(&value.name) {
            Some(current_value) => {
                *current_value = value;
            }
            None => {
                self.headers.push(value);
            }
        }
    }

    /// Remove a raw header from `Headers`, returning it
    ///
    /// Returns `None` if `name` isn't present in `Headers`.
    pub fn remove_raw(&mut self, name: &str) -> Option<HeaderValue> {
        self.find_header_index(name).map(|i| self.headers.remove(i))
    }

    pub(crate) fn find_header(&self, name: &str) -> Option<&HeaderValue> {
        self.headers.iter().find(|value| name == value.name)
    }

    fn find_header_mut(&mut self, name: &str) -> Option<&mut HeaderValue> {
        self.headers.iter_mut().find(|value| name == value.name)
    }

    fn find_header_index(&self, name: &str) -> Option<usize> {
        self.headers
            .iter()
            .enumerate()
            .find(|(_i, value)| name == value.name)
            .map(|(i, _)| i)
    }
}

impl Display for Headers {
    /// Formats `Headers`, ready to put them into an email
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for value in &self.headers {
            f.write_str(&value.name)?;
            f.write_str(": ")?;
            f.write_str(&value.encoded_value)?;
            f.write_str("\r\n")?;
        }

        Ok(())
    }
}

/// A possible error when converting a `HeaderName` from another type.
// comes from `http` crate
#[allow(missing_copy_implementations)]
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct InvalidHeaderName;

impl fmt::Display for InvalidHeaderName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid header name")
    }
}

impl Error for InvalidHeaderName {}

/// A valid header name
#[derive(Debug, Clone)]
pub struct HeaderName(Cow<'static, str>);

impl HeaderName {
    /// Creates a new header name
    pub fn new_from_ascii(ascii: String) -> Result<Self, InvalidHeaderName> {
        if !ascii.is_empty() && ascii.len() <= 76 && ascii.is_ascii() && !ascii.contains([':', ' '])
        {
            Ok(Self(Cow::Owned(ascii)))
        } else {
            Err(InvalidHeaderName)
        }
    }

    /// Creates a new header name, panics on invalid name
    pub const fn new_from_ascii_str(ascii: &'static str) -> Self {
        assert!(!ascii.is_empty());
        assert!(ascii.len() <= 76);
        assert!(ascii.is_ascii());

        let bytes = ascii.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            assert!(bytes[i] != b' ');
            assert!(bytes[i] != b':');

            i += 1;
        }

        Self(Cow::Borrowed(ascii))
    }
}

impl Display for HeaderName {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self)
    }
}

impl Deref for HeaderName {
    type Target = str;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<[u8]> for HeaderName {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        let s: &str = self.as_ref();
        s.as_bytes()
    }
}

impl AsRef<str> for HeaderName {
    #[inline]
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl PartialEq<HeaderName> for HeaderName {
    fn eq(&self, other: &HeaderName) -> bool {
        self.eq_ignore_ascii_case(other)
    }
}

impl PartialEq<&str> for HeaderName {
    fn eq(&self, other: &&str) -> bool {
        self.eq_ignore_ascii_case(other)
    }
}

impl PartialEq<HeaderName> for &str {
    fn eq(&self, other: &HeaderName) -> bool {
        self.eq_ignore_ascii_case(other)
    }
}

/// A safe for use header value
#[derive(Debug, Clone, PartialEq)]
pub struct HeaderValue {
    name: HeaderName,
    raw_value: String,
    encoded_value: String,
}

impl HeaderValue {
    /// Construct a new `HeaderValue` and encode it
    ///
    /// Takes the header `name` and the `raw_value` and encodes
    /// it via `RFC2047` and line folds it.
    ///
    /// [`RFC2047`]: https://datatracker.ietf.org/doc/html/rfc2047
    pub fn new(name: HeaderName, raw_value: String) -> Self {
        let mut encoded_value = String::with_capacity(raw_value.len());
        HeaderValueEncoder::encode(&name, &raw_value, &mut encoded_value).unwrap();

        Self {
            name,
            raw_value,
            encoded_value,
        }
    }

    /// Construct a new `HeaderValue` using a pre-encoded header value
    ///
    /// This method is _extremely_ dangerous as it opens up
    /// the encoder to header injection attacks, but is sometimes
    /// acceptable for use if `encoded_value` contains only ascii
    /// printable characters and is already line folded.
    ///
    /// When in doubt, use [`HeaderValue::new`].
    pub fn dangerous_new_pre_encoded(
        name: HeaderName,
        raw_value: String,
        encoded_value: String,
    ) -> Self {
        Self {
            name,
            raw_value,
            encoded_value,
        }
    }

    #[cfg(feature = "dkim")]
    pub(crate) fn get_raw(&self) -> &str {
        &self.raw_value
    }

    #[cfg(feature = "dkim")]
    pub(crate) fn get_encoded(&self) -> &str {
        &self.encoded_value
    }
}

/// [RFC 1522](https://tools.ietf.org/html/rfc1522) header value encoder
struct HeaderValueEncoder<'a> {
    writer: EmailWriter<'a>,
    encode_buf: String,
}

impl<'a> HeaderValueEncoder<'a> {
    fn encode(name: &str, value: &'a str, f: &'a mut impl fmt::Write) -> fmt::Result {
        let encoder = Self::new(name, f);
        encoder.format(value.split_inclusive(' '))
    }

    fn new(name: &str, writer: &'a mut dyn Write) -> Self {
        let line_len = name.len() + ": ".len();
        let writer = EmailWriter::new(writer, line_len, 0, false);

        Self {
            writer,
            encode_buf: String::new(),
        }
    }

    fn format(mut self, words_iter: impl Iterator<Item = &'a str>) -> fmt::Result {
        for next_word in words_iter {
            let allowed = allowed_str(next_word);

            if allowed {
                // This word only contains allowed characters

                // the next word is allowed, but we may have accumulated some words to encode
                self.flush_encode_buf()?;

                self.writer.folding().write_str(next_word)?;
            } else {
                // This word contains unallowed characters
                self.encode_buf.push_str(next_word);
            }
        }

        self.flush_encode_buf()?;

        Ok(())
    }

    fn flush_encode_buf(&mut self) -> fmt::Result {
        if self.encode_buf.is_empty() {
            // nothing to encode
            return Ok(());
        }

        let prefix = self.encode_buf.trim_end_matches(' ');
        email_encoding::headers::rfc2047::encode(prefix, &mut self.writer)?;

        // TODO: add a better API for doing this in email-encoding
        let spaces = self.encode_buf.len() - prefix.len();
        for _ in 0..spaces {
            self.writer.space();
        }

        self.encode_buf.clear();
        Ok(())
    }
}

fn allowed_str(s: &str) -> bool {
    s.bytes().all(allowed_char)
}

const fn allowed_char(c: u8) -> bool {
    c >= 1 && c <= 9 || c == 11 || c == 12 || c >= 14 && c <= 127
}
