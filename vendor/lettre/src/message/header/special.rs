use crate::{
    BoxError,
    message::header::{Header, HeaderName, HeaderValue},
};

/// Message format version, defined in [RFC2045](https://tools.ietf.org/html/rfc2045#section-4)
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct MimeVersion {
    major: u8,
    minor: u8,
}

/// MIME version 1.0
///
/// Should be used in all MIME messages.
pub const MIME_VERSION_1_0: MimeVersion = MimeVersion::new(1, 0);

impl MimeVersion {
    /// Build a new `MimeVersion` header
    pub const fn new(major: u8, minor: u8) -> Self {
        MimeVersion { major, minor }
    }

    /// Get the `major` value of this `MimeVersion` header.
    #[inline]
    pub const fn major(self) -> u8 {
        self.major
    }

    /// Get the `minor` value of this `MimeVersion` header.
    #[inline]
    pub const fn minor(self) -> u8 {
        self.minor
    }
}

impl Header for MimeVersion {
    fn name() -> HeaderName {
        HeaderName::new_from_ascii_str("MIME-Version")
    }

    fn parse(s: &str) -> Result<Self, BoxError> {
        let mut s = s.split('.');

        let major = s
            .next()
            .expect("The first call to next for a Split<char> always succeeds");
        let minor = s
            .next()
            .ok_or_else(|| String::from("MIME-Version header doesn't contain '.'"))?;
        let major = major.parse()?;
        let minor = minor.parse()?;
        Ok(MimeVersion::new(major, minor))
    }

    fn display(&self) -> HeaderValue {
        let val = format!("{}.{}", self.major, self.minor);
        HeaderValue::dangerous_new_pre_encoded(Self::name(), val.clone(), val)
    }
}

impl Default for MimeVersion {
    fn default() -> Self {
        MIME_VERSION_1_0
    }
}
