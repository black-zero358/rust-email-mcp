use std::{mem, ops::Deref};

use crate::message::header::ContentTransferEncoding;

/// A [`Message`][super::Message] or [`SinglePart`][super::SinglePart] body that has already been encoded.
#[derive(Debug, Clone)]
pub struct Body {
    buf: Vec<u8>,
    encoding: ContentTransferEncoding,
}

/// Either a `Vec<u8>` or a `String`.
///
/// If the content is valid utf-8 a `String` should be passed, as it
/// makes for a more efficient `Content-Transfer-Encoding` to be chosen.
#[derive(Debug, Clone)]
pub enum MaybeString {
    /// Binary data
    Binary(Vec<u8>),
    /// UTF-8 string
    String(String),
}

impl Body {
    /// Encode the supplied `buf`, making it ready to be sent as a body.
    ///
    /// Takes a `Vec<u8>` or a `String`.
    ///
    /// Automatically chooses the most efficient encoding between
    /// `7bit`, `quoted-printable` and `base64`.
    ///
    /// If `String` is passed, line endings are converted to `CRLF`.
    ///
    /// If `buf` is valid utf-8 a `String` should be supplied, as `String`s
    /// can be encoded as `7bit` or `quoted-printable`, while `Vec<u8>` always
    /// get encoded as `base64`.
    pub fn new<B: Into<MaybeString>>(buf: B) -> Self {
        let mut buf: MaybeString = buf.into();

        let encoding = buf.encoding(false);
        buf.encode_crlf();
        Self::new_impl(buf.into(), encoding)
    }

    /// Encode the supplied `buf`, using the provided `encoding`.
    ///
    /// [`Body::new`] is generally the better option.
    ///
    /// If `String` is passed, line endings are converted to `CRLF`.
    ///
    /// Returns an [`Err`] giving back the supplied `buf`, in case the chosen
    /// encoding would have resulted into `buf` being encoded
    /// into an invalid body.
    pub fn new_with_encoding<B: Into<MaybeString>>(
        buf: B,
        encoding: ContentTransferEncoding,
    ) -> Result<Self, Vec<u8>> {
        let mut buf: MaybeString = buf.into();

        let best_encoding = buf.encoding(true);
        let ok = match (encoding, best_encoding) {
            (ContentTransferEncoding::SevenBit, ContentTransferEncoding::SevenBit) => true,
            (
                ContentTransferEncoding::EightBit,
                ContentTransferEncoding::SevenBit | ContentTransferEncoding::EightBit,
            ) => true,
            (ContentTransferEncoding::SevenBit | ContentTransferEncoding::EightBit, _) => false,
            (
                ContentTransferEncoding::QuotedPrintable
                | ContentTransferEncoding::Base64
                | ContentTransferEncoding::Binary,
                _,
            ) => true,
        };
        if !ok {
            return Err(buf.into());
        }

        buf.encode_crlf();
        Ok(Self::new_impl(buf.into(), encoding))
    }

    /// Builds a new `Body` using a pre-encoded buffer.
    ///
    /// **Generally not you want.**
    ///
    /// `buf` shouldn't contain non-ascii characters, lines longer than 1000 characters or nul bytes.
    #[inline]
    pub fn dangerous_pre_encoded(buf: Vec<u8>, encoding: ContentTransferEncoding) -> Self {
        Self { buf, encoding }
    }

    /// Encodes the supplied `buf` using the provided `encoding`
    fn new_impl(buf: Vec<u8>, encoding: ContentTransferEncoding) -> Self {
        match encoding {
            ContentTransferEncoding::SevenBit
            | ContentTransferEncoding::EightBit
            | ContentTransferEncoding::Binary => Self { buf, encoding },
            ContentTransferEncoding::QuotedPrintable => {
                let encoded = quoted_printable::encode(buf);

                Self::dangerous_pre_encoded(encoded, ContentTransferEncoding::QuotedPrintable)
            }
            ContentTransferEncoding::Base64 => {
                let len = email_encoding::body::base64::encoded_len(buf.len());

                let mut out = String::with_capacity(len);
                email_encoding::body::base64::encode(&buf, &mut out)
                    .expect("encode body as base64");

                Self::dangerous_pre_encoded(out.into_bytes(), ContentTransferEncoding::Base64)
            }
        }
    }

    /// Returns the length of this `Body` in bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Returns `true` if this `Body` has a length of zero, `false` otherwise.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Returns the `Content-Transfer-Encoding` of this `Body`.
    #[inline]
    pub fn encoding(&self) -> ContentTransferEncoding {
        self.encoding
    }

    /// Consumes `Body` and returns the inner `Vec<u8>`
    #[inline]
    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }
}

impl MaybeString {
    /// Suggests the best `Content-Transfer-Encoding` to be used for this `MaybeString`
    ///
    /// The `binary` encoding is never returned
    fn encoding(&self, supports_utf8: bool) -> ContentTransferEncoding {
        use email_encoding::body::Encoding;

        let output = match self {
            Self::String(s) => Encoding::choose(s.as_str(), supports_utf8),
            Self::Binary(b) => Encoding::choose(b.as_slice(), supports_utf8),
        };

        match output {
            Encoding::SevenBit => ContentTransferEncoding::SevenBit,
            Encoding::EightBit => ContentTransferEncoding::EightBit,
            Encoding::QuotedPrintable => ContentTransferEncoding::QuotedPrintable,
            Encoding::Base64 => ContentTransferEncoding::Base64,
        }
    }

    /// Encode line endings to CRLF if the variant is `String`
    fn encode_crlf(&mut self) {
        match self {
            Self::String(string) => in_place_crlf_line_endings(string),
            Self::Binary(_) => {}
        }
    }
}

/// A trait for something that takes an encoded [`Body`].
///
/// Used by [`MessageBuilder::body`][super::MessageBuilder::body] and
/// [`SinglePartBuilder::body`][super::SinglePartBuilder::body],
/// which can either take something that can be encoded into [`Body`]
/// or a pre-encoded [`Body`].
///
/// If `encoding` is `None` the best encoding between `7bit`, `quoted-printable`
/// and `base64` is chosen based on the input body. **Best option.**
///
/// If `encoding` is `Some` the supplied encoding is used.
/// **NOTE:** if using the specified `encoding` would result into a malformed
/// body, this will panic!
pub trait IntoBody {
    /// Encode as valid body
    fn into_body(self, encoding: Option<ContentTransferEncoding>) -> Body;
}

impl<T> IntoBody for T
where
    T: Into<MaybeString>,
{
    fn into_body(self, encoding: Option<ContentTransferEncoding>) -> Body {
        match encoding {
            Some(encoding) => Body::new_with_encoding(self, encoding).expect("invalid encoding"),
            None => Body::new(self),
        }
    }
}

impl IntoBody for Body {
    fn into_body(self, encoding: Option<ContentTransferEncoding>) -> Body {
        let _ = encoding;

        self
    }
}

impl AsRef<[u8]> for Body {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.buf.as_ref()
    }
}

impl From<Vec<u8>> for MaybeString {
    #[inline]
    fn from(b: Vec<u8>) -> Self {
        Self::Binary(b)
    }
}

impl From<String> for MaybeString {
    #[inline]
    fn from(s: String) -> Self {
        Self::String(s)
    }
}

impl From<MaybeString> for Vec<u8> {
    #[inline]
    fn from(s: MaybeString) -> Self {
        match s {
            MaybeString::Binary(b) => b,
            MaybeString::String(s) => s.into(),
        }
    }
}

impl Deref for MaybeString {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &Self::Target {
        match self {
            Self::Binary(b) => b.as_ref(),
            Self::String(s) => s.as_ref(),
        }
    }
}

/// In place conversion to CRLF line endings
fn in_place_crlf_line_endings(string: &mut String) {
    let indices = find_all_lf_char_indices(string);

    for i in indices {
        // this relies on `indices` being in reverse order
        string.insert(i, '\r');
    }
}

/// Find indices to all places where `\r` should be inserted
/// in order to make `s` have CRLF line endings
///
/// The list is reversed, which is more efficient.
fn find_all_lf_char_indices(s: &str) -> Vec<usize> {
    let mut indices = Vec::new();

    let mut found_lf = false;
    for (i, c) in s.char_indices().rev() {
        if mem::take(&mut found_lf) && c != '\r' {
            // the previous character was `\n`, but this isn't a `\r`
            indices.push(i + c.len_utf8());
        }

        found_lf = c == '\n';
    }

    if found_lf {
        // the first character is `\n`
        indices.push(0);
    }

    indices
}
