//! Provides limited SASL authentication mechanisms

use std::fmt::{self, Debug, Display, Formatter};

use crate::transport::smtp::error::{self, Error};

/// Accepted authentication mechanisms
///
/// Trying LOGIN last as it is deprecated.
pub const DEFAULT_MECHANISMS: &[Mechanism] = &[Mechanism::Plain, Mechanism::Login];

/// Contains user credentials
#[derive(PartialEq, Eq, Clone, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Credentials {
    authentication_identity: String,
    secret: String,
}

impl Credentials {
    /// Create a `Credentials` struct from username and password
    pub fn new(username: String, password: String) -> Credentials {
        Credentials {
            authentication_identity: username,
            secret: password,
        }
    }
}

impl<S, T> From<(S, T)> for Credentials
where
    S: Into<String>,
    T: Into<String>,
{
    fn from((username, password): (S, T)) -> Self {
        Credentials::new(username.into(), password.into())
    }
}

impl Debug for Credentials {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials").finish()
    }
}

/// Represents authentication mechanisms
#[derive(PartialEq, Eq, Copy, Clone, Hash, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Mechanism {
    /// PLAIN authentication mechanism, defined in
    /// [RFC 4616](https://tools.ietf.org/html/rfc4616)
    Plain,
    /// LOGIN authentication mechanism
    /// Obsolete but needed for some providers (like Office 365)
    ///
    /// Defined in [draft-murchison-sasl-login-00](https://www.ietf.org/archive/id/draft-murchison-sasl-login-00.txt).
    Login,
    /// Non-standard XOAUTH2 mechanism, defined in
    /// [xoauth2-protocol](https://developers.google.com/gmail/imap/xoauth2-protocol)
    Xoauth2,
}

impl Display for Mechanism {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(match *self {
            Mechanism::Plain => "PLAIN",
            Mechanism::Login => "LOGIN",
            Mechanism::Xoauth2 => "XOAUTH2",
        })
    }
}

impl Mechanism {
    /// Does the mechanism support initial response?
    pub fn supports_initial_response(self) -> bool {
        match self {
            Mechanism::Plain | Mechanism::Xoauth2 => true,
            Mechanism::Login => false,
        }
    }

    /// Returns the string to send to the server, using the provided username, password and
    /// challenge in some cases
    pub fn response(
        self,
        credentials: &Credentials,
        challenge: Option<&str>,
    ) -> Result<String, Error> {
        match self {
            Mechanism::Plain => match challenge {
                Some(_) => Err(error::client("This mechanism does not expect a challenge")),
                None => Ok(format!(
                    "\u{0}{}\u{0}{}",
                    credentials.authentication_identity, credentials.secret
                )),
            },
            Mechanism::Login => {
                let decoded_challenge = challenge
                    .ok_or_else(|| error::client("This mechanism does expect a challenge"))?;

                if contains_ignore_ascii_case(
                    decoded_challenge,
                    ["User Name", "Username:", "Username", "User Name\0"],
                ) {
                    return Ok(credentials.authentication_identity.clone());
                }

                if contains_ignore_ascii_case(
                    decoded_challenge,
                    ["Password", "Password:", "Password\0"],
                ) {
                    return Ok(credentials.secret.clone());
                }

                Err(error::client("Unrecognized challenge"))
            }
            Mechanism::Xoauth2 => match challenge {
                Some(_) => Err(error::client("This mechanism does not expect a challenge")),
                None => Ok(format!(
                    "user={}\x01auth=Bearer {}\x01\x01",
                    credentials.authentication_identity, credentials.secret
                )),
            },
        }
    }
}

fn contains_ignore_ascii_case<'a>(
    haystack: &str,
    needles: impl IntoIterator<Item = &'a str>,
) -> bool {
    needles
        .into_iter()
        .any(|item| item.eq_ignore_ascii_case(haystack))
}
