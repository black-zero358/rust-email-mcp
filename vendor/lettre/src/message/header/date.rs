use std::time::SystemTime;

use httpdate::HttpDate;

use super::{Header, HeaderName, HeaderValue};
use crate::BoxError;

/// Message `Date` header
///
/// Defined in [RFC2822](https://tools.ietf.org/html/rfc2822#section-3.3)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Date(HttpDate);

impl Date {
    /// Build a `Date` from [`SystemTime`]
    pub fn new(st: SystemTime) -> Self {
        Self(st.into())
    }

    /// Get the current date
    ///
    /// Shortcut for `Date::new(SystemTime::now())`
    pub fn now() -> Self {
        Self::new(crate::time::now())
    }
}

impl Header for Date {
    fn name() -> HeaderName {
        HeaderName::new_from_ascii_str("Date")
    }

    fn parse(s: &str) -> Result<Self, BoxError> {
        let mut s = String::from(s);
        if s.ends_with("+0000") {
            // The httpdate crate expects the `Date` to end in ` GMT`, but email
            // uses `+0000` to indicate UTC, so we crudely fix this issue here.

            s.truncate(s.len() - "+0000".len());
            s.push_str("GMT");
        }

        Ok(Self(s.parse::<HttpDate>()?))
    }

    fn display(&self) -> HeaderValue {
        let mut val = self.0.to_string();
        if val.ends_with(" GMT") {
            // The httpdate crate always appends ` GMT` to the end of the string,
            // but this is considered an obsolete date format for email
            // https://tools.ietf.org/html/rfc2822#appendix-A.6.2,
            // so we replace `GMT` with `+0000`
            val.truncate(val.len() - "GMT".len());
            val.push_str("+0000");
        }

        HeaderValue::dangerous_new_pre_encoded(Self::name(), val.clone(), val)
    }
}

impl From<SystemTime> for Date {
    fn from(st: SystemTime) -> Self {
        Self::new(st)
    }
}

impl From<Date> for SystemTime {
    fn from(this: Date) -> SystemTime {
        this.0.into()
    }
}
