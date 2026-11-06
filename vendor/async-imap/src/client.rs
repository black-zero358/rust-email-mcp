use std::collections::{HashMap, HashSet};
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::str;

use async_channel::{self as channel, bounded};
#[cfg(feature = "runtime-async-std")]
use async_std::io::{Read, Write, WriteExt};
use base64::Engine as _;
use extensions::id::{format_identification, parse_id};
use extensions::quota::parse_get_quota_root;
use futures_util::{Stream, TryStreamExt, io};
use imap_proto::rfc5464::Metadata;
use imap_proto::{MailboxDatum, RequestId, Response, Status};
#[cfg(feature = "runtime-tokio")]
use tokio::io::{AsyncRead as Read, AsyncWrite as Write, AsyncWriteExt};

use super::authenticator::Authenticator;
use super::error::{Error, ParseError, Result, ValidateError};
use super::parse::*;
use super::types::*;
use crate::extensions::{self, quota::parse_get_quota};
use crate::imap_stream::ImapStream;

macro_rules! quote {
    ($x:expr) => {
        format!("\"{}\"", $x.replace(r"\", r"\\").replace("\"", "\\\""))
    };
}

/// An authenticated IMAP session providing the usual IMAP commands. This type is what you get from
/// a succesful login attempt.
///
/// Note that the server *is* allowed to unilaterally send things to the client for messages in
/// a selected mailbox whose status has changed. See the note on [unilateral server responses
/// in RFC 3501](https://tools.ietf.org/html/rfc3501#section-7). Any such messages are parsed out
/// and sent on `Session::unsolicited_responses`.
// Both `Client` and `Session` deref to [`Connection`](struct.Connection.html), the underlying
// primitives type.
#[derive(Debug)]
pub struct Session<T: Read + Write + Unpin + fmt::Debug> {
    pub(crate) conn: Connection<T>,
    pub(crate) unsolicited_responses_tx: channel::Sender<UnsolicitedResponse>,

    /// Server responses that are not related to the current command. See also the note on
    /// [unilateral server responses in RFC 3501](https://tools.ietf.org/html/rfc3501#section-7).
    pub unsolicited_responses: channel::Receiver<UnsolicitedResponse>,
}

impl<T: Read + Write + Unpin + fmt::Debug> Unpin for Session<T> {}
impl<T: Read + Write + Unpin + fmt::Debug> Unpin for Client<T> {}
impl<T: Read + Write + Unpin + fmt::Debug> Unpin for Connection<T> {}

// Make it possible to access the inner connection and modify its settings, such as read/write
// timeouts.
impl<T: Read + Write + Unpin + fmt::Debug> AsMut<T> for Session<T> {
    fn as_mut(&mut self) -> &mut T {
        self.conn.stream.as_mut()
    }
}

/// An (unauthenticated) handle to talk to an IMAP server.
///
/// This is what you get when first
/// connecting. A succesfull call to [`Client::login`] or [`Client::authenticate`] will return a
/// [`Session`] instance that provides the usual IMAP methods.
// Both `Client` and `Session` deref to [`Connection`](struct.Connection.html), the underlying
// primitives type.
#[derive(Debug)]
pub struct Client<T: Read + Write + Unpin + fmt::Debug> {
    conn: Connection<T>,
}

/// The underlying primitives type. Both `Client`(unauthenticated) and `Session`(after succesful
/// login) use a `Connection` internally for the TCP stream primitives.
#[derive(Debug)]
pub struct Connection<T: Read + Write + Unpin + fmt::Debug> {
    pub(crate) stream: ImapStream<T>,

    /// Manages the request ids.
    pub(crate) request_ids: IdGenerator,
}

// `Deref` instances are so we can make use of the same underlying primitives in `Client` and
// `Session`
impl<T: Read + Write + Unpin + fmt::Debug> Deref for Client<T> {
    type Target = Connection<T>;

    fn deref(&self) -> &Connection<T> {
        &self.conn
    }
}

impl<T: Read + Write + Unpin + fmt::Debug> DerefMut for Client<T> {
    fn deref_mut(&mut self) -> &mut Connection<T> {
        &mut self.conn
    }
}

impl<T: Read + Write + Unpin + fmt::Debug> Deref for Session<T> {
    type Target = Connection<T>;

    fn deref(&self) -> &Connection<T> {
        &self.conn
    }
}

impl<T: Read + Write + Unpin + fmt::Debug> DerefMut for Session<T> {
    fn deref_mut(&mut self) -> &mut Connection<T> {
        &mut self.conn
    }
}

// As the pattern of returning the unauthenticated `Client` (a.k.a. `self`) back with a login error
// is relatively common, it's abstacted away into a macro here.
//
// Note: 1) using `.map_err(|e| (e, self))` or similar here makes the closure own self, so we can't
//          do that.
//       2) in theory we wouldn't need the second parameter, and could just use the identifier
//          `self` from the surrounding function, but being explicit here seems a lot cleaner.
macro_rules! ok_or_unauth_client_err {
    ($r:expr, $self:expr) => {
        match $r {
            Ok(o) => o,
            Err(e) => return Err((e.into(), $self)),
        }
    };
}

impl<T: Read + Write + Unpin + fmt::Debug + Send> Client<T> {
    /// Creates a new client over the given stream.
    ///
    /// This method primarily exists for writing tests that mock the underlying transport, but can
    /// also be used to support IMAP over custom tunnels.
    pub fn new(stream: T) -> Client<T> {
        Self::with_buffer_limit(stream, 512 * 1024 * 1024)
    }

    /// Creates a client with a per-response parser buffer limit in bytes.
    /// The limit is clamped to 1..=512 MiB and applies before allocation.
    /// Transport byte accounting remains the caller's responsibility.
    pub fn with_buffer_limit(stream: T, max_buffer: usize) -> Client<T> {
        let stream = ImapStream::with_buffer_limit(stream, max_buffer);

        Client {
            conn: Connection {
                stream,
                request_ids: IdGenerator::new(),
            },
        }
    }

    /// Convert this Client into the raw underlying stream.
    pub fn into_inner(self) -> T {
        let Self { conn, .. } = self;
        conn.into_inner()
    }

    /// Log in to the IMAP server. Upon success a [`Session`](struct.Session.html) instance is
    /// returned; on error the original `Client` instance is returned in addition to the error.
    /// This is because `login` takes ownership of `self`, so in order to try again (e.g. after
    /// prompting the user for credetials), ownership of the original `Client` needs to be
    /// transferred back to the caller.
    ///
    /// ```ignore
    /// # fn main() -> async_imap::error::Result<()> {
    /// # async_std::task::block_on(async {
    ///
    /// let tls = async_native_tls::TlsConnector::new();
    /// let client = async_imap::connect(
    ///     ("imap.example.org", 993),
    ///     "imap.example.org",
    ///     tls
    /// ).await?;
    ///
    /// match client.login("user", "pass").await {
    ///     Ok(s) => {
    ///         // you are successfully authenticated!
    ///     },
    ///     Err((e, orig_client)) => {
    ///         eprintln!("error logging in: {}", e);
    ///         // prompt user and try again with orig_client here
    ///         return Err(e);
    ///     }
    /// }
    ///
    /// # Ok(())
    /// # }) }
    /// ```
    pub async fn login<U: AsRef<str>, P: AsRef<str>>(
        self,
        username: U,
        password: P,
    ) -> ::std::result::Result<Session<T>, (Error, Client<T>)> {
        let (session, _capabilities) = self.login_with_capabilities(username, password).await?;
        Ok(session)
    }

    /// Logs in to the IMAP server.
    ///
    /// Upon sucess returns a [`Session`] instance
    /// and optional capabilities if
    /// the response contained `CAPABILITY` response code.
    pub async fn login_with_capabilities<U: AsRef<str>, P: AsRef<str>>(
        mut self,
        username: U,
        password: P,
    ) -> ::std::result::Result<(Session<T>, Option<Capabilities>), (Error, Client<T>)> {
        let u = ok_or_unauth_client_err!(validate_str(username.as_ref()), self);
        let p = ok_or_unauth_client_err!(validate_str(password.as_ref()), self);

        let id = ok_or_unauth_client_err!(self.run_command(&format!("LOGIN {u} {p}")).await, self);
        loop {
            let Some(res) = ok_or_unauth_client_err!(self.stream.try_next().await, self) else {
                return Err((Error::ConnectionLost, self));
            };

            if let Response::Done {
                status,
                outcome,
                tag,
            } = res.parsed()
                && *tag == id
            {
                ok_or_unauth_client_err!(
                    self.check_status_ok(
                        status,
                        outcome.code.as_ref(),
                        outcome.information.as_deref()
                    ),
                    self
                );

                let capabilities =
                    if let Some(imap_proto::ResponseCode::Capabilities(capabilities)) =
                        &outcome.code
                    {
                        use crate::types::{Capabilities, Capability};
                        let capability_set: HashSet<Capability> =
                            capabilities.iter().map(Capability::from).collect();
                        Some(Capabilities(capability_set))
                    } else {
                        None
                    };
                return Ok((Session::new(self.conn), capabilities));
            }
        }
    }

    /// Authenticate with the server using the given custom `authenticator` to handle the server's
    /// challenge.
    ///
    /// ```ignore
    /// struct OAuth2 {
    ///     user: String,
    ///     access_token: String,
    /// }
    ///
    /// impl async_imap::Authenticator for &OAuth2 {
    ///     type Response = String;
    ///     fn process(&mut self, _: &[u8]) -> Self::Response {
    ///         format!(
    ///             "user={}\x01auth=Bearer {}\x01\x01",
    ///             self.user, self.access_token
    ///         )
    ///     }
    /// }
    ///
    /// # fn main() -> async_imap::error::Result<()> {
    /// # async_std::task::block_on(async {
    ///
    ///     let auth = OAuth2 {
    ///         user: String::from("me@example.com"),
    ///         access_token: String::from("<access_token>"),
    ///     };
    ///
    ///     let domain = "imap.example.com";
    ///     let tls = async_native_tls::TlsConnector::new();
    ///     let client = async_imap::connect((domain, 993), domain, tls).await?;
    ///     match client.authenticate("XOAUTH2", &auth).await {
    ///         Ok(session) => {
    ///             // you are successfully authenticated!
    ///         },
    ///         Err((err, orig_client)) => {
    ///             eprintln!("error authenticating: {}", err);
    ///             // prompt user and try again with orig_client here
    ///             return Err(err);
    ///         }
    ///     };
    /// # Ok(())
    /// # }) }
    /// ```
    pub async fn authenticate<A: Authenticator, S: AsRef<str>>(
        mut self,
        auth_type: S,
        authenticator: A,
    ) -> ::std::result::Result<Session<T>, (Error, Client<T>)> {
        let id = ok_or_unauth_client_err!(
            self.run_command(&format!("AUTHENTICATE {}", auth_type.as_ref()))
                .await,
            self
        );
        let session = self.do_auth_handshake(id, authenticator).await?;
        Ok(session)
    }

    /// This func does the handshake process once the authenticate command is made.
    async fn do_auth_handshake<A: Authenticator>(
        mut self,
        id: RequestId,
        mut authenticator: A,
    ) -> ::std::result::Result<Session<T>, (Error, Client<T>)> {
        // explicit match blocks neccessary to convert error to tuple and not bind self too
        // early (see also comment on `login`)
        loop {
            let Some(res) = ok_or_unauth_client_err!(self.read_response().await, self) else {
                return Err((Error::ConnectionLost, self));
            };
            match res.parsed() {
                Response::Continue(outcome) => {
                    let challenge = if let Some(text) = &outcome.information {
                        ok_or_unauth_client_err!(
                            base64::engine::general_purpose::STANDARD
                                .decode(text.as_ref())
                                .map_err(|e| Error::Parse(ParseError::Authentication(
                                    (*text).to_string(),
                                    Some(e)
                                ))),
                            self
                        )
                    } else {
                        Vec::new()
                    };
                    let raw_response = &mut authenticator.process(&challenge);
                    let auth_response =
                        base64::engine::general_purpose::STANDARD.encode(raw_response);

                    ok_or_unauth_client_err!(
                        self.conn.run_command_untagged(&auth_response).await,
                        self
                    );
                }
                _ => {
                    ok_or_unauth_client_err!(self.check_done_ok_from(&id, None, res).await, self);
                    return Ok(Session::new(self.conn));
                }
            }
        }
    }
}

impl<T: Read + Write + Unpin + fmt::Debug + Send> Session<T> {
    unsafe_pinned!(conn: Connection<T>);

    pub(crate) fn get_stream(self: Pin<&mut Self>) -> Pin<&mut ImapStream<T>> {
        self.conn().stream()
    }

    // not public, just to avoid duplicating the channel creation code
    fn new(conn: Connection<T>) -> Self {
        let (tx, rx) = bounded(100);
        Session {
            conn,
            unsolicited_responses: rx,
            unsolicited_responses_tx: tx,
        }
    }

    /// Selects a mailbox.
    ///
    /// The `SELECT` command selects a mailbox so that messages in the mailbox can be accessed.
    /// Note that earlier versions of this protocol only required the FLAGS, EXISTS, and RECENT
    /// untagged data; consequently, client implementations SHOULD implement default behavior for
    /// missing data as discussed with the individual item.
    ///
    /// Only one mailbox can be selected at a time in a connection; simultaneous access to multiple
    /// mailboxes requires multiple connections.  The `SELECT` command automatically deselects any
    /// currently selected mailbox before attempting the new selection. Consequently, if a mailbox
    /// is selected and a `SELECT` command that fails is attempted, no mailbox is selected.
    ///
    /// Note that the server *is* allowed to unilaterally send things to the client for messages in
    /// a selected mailbox whose status has changed. See the note on [unilateral server responses
    /// in RFC 3501](https://tools.ietf.org/html/rfc3501#section-7). This means that if run commands,
    /// you *may* see additional untagged `RECENT`, `EXISTS`, `FETCH`, and `EXPUNGE` responses.
    /// You can get them from the `unsolicited_responses` channel of the [`Session`](struct.Session.html).
    pub async fn select<S: AsRef<str>>(&mut self, mailbox_name: S) -> Result<Mailbox> {
        // TODO: also note READ/WRITE vs READ-only mode!
        let id = self
            .run_command(&format!("SELECT {}", validate_str(mailbox_name.as_ref())?))
            .await?;
        let mbox = parse_mailbox(
            &mut self.conn.stream,
            self.unsolicited_responses_tx.clone(),
            id,
        )
        .await?;

        Ok(mbox)
    }

    /// Selects a mailbox with `(CONDSTORE)` parameter as defined in
    /// [RFC 7162](https://www.rfc-editor.org/rfc/rfc7162.html#section-3.1.8).
    pub async fn select_condstore<S: AsRef<str>>(&mut self, mailbox_name: S) -> Result<Mailbox> {
        let id = self
            .run_command(&format!(
                "SELECT {} (CONDSTORE)",
                validate_str(mailbox_name.as_ref())?
            ))
            .await?;
        let mbox = parse_mailbox(
            &mut self.conn.stream,
            self.unsolicited_responses_tx.clone(),
            id,
        )
        .await?;

        Ok(mbox)
    }

    /// The `EXAMINE` command is identical to [`Session::select`] and returns the same output;
    /// however, the selected mailbox is identified as read-only. No changes to the permanent state
    /// of the mailbox, including per-user state, will happen in a mailbox opened with `examine`;
    /// in particular, messagess cannot lose [`Flag::Recent`] in an examined mailbox.
    pub async fn examine<S: AsRef<str>>(&mut self, mailbox_name: S) -> Result<Mailbox> {
        let id = self
            .run_command(&format!("EXAMINE {}", validate_str(mailbox_name.as_ref())?))
            .await?;
        let mbox = parse_mailbox(
            &mut self.conn.stream,
            self.unsolicited_responses_tx.clone(),
            id,
        )
        .await?;

        Ok(mbox)
    }

    /// Fetch retreives data associated with a set of messages in the mailbox.
    ///
    /// Note that the server *is* allowed to unilaterally include `FETCH` responses for other
    /// messages in the selected mailbox whose status has changed. See the note on [unilateral
    /// server responses in RFC 3501](https://tools.ietf.org/html/rfc3501#section-7).
    ///
    /// `query` is a list of "data items" (space-separated in parentheses if `>1`). There are three
    /// "macro items" which specify commonly-used sets of data items, and can be used instead of
    /// data items.  A macro must be used by itself, and not in conjunction with other macros or
    /// data items. They are:
    ///
    ///  - `ALL`: equivalent to: `(FLAGS INTERNALDATE RFC822.SIZE ENVELOPE)`
    ///  - `FAST`: equivalent to: `(FLAGS INTERNALDATE RFC822.SIZE)`
    ///
    /// The currently defined data items that can be fetched are listen [in the
    /// RFC](https://tools.ietf.org/html/rfc3501#section-6.4.5), but here are some common ones:
    ///
    ///  - `FLAGS`: The flags that are set for this message.
    ///  - `INTERNALDATE`: The internal date of the message.
    ///  - `BODY[<section>]`:
    ///
    ///    The text of a particular body section.  The section specification is a set of zero or
    ///    more part specifiers delimited by periods.  A part specifier is either a part number
    ///    (see RFC) or one of the following: `HEADER`, `HEADER.FIELDS`, `HEADER.FIELDS.NOT`,
    ///    `MIME`, and `TEXT`.  An empty section specification (i.e., `BODY[]`) refers to the
    ///    entire message, including the header.
    ///
    ///    The `HEADER`, `HEADER.FIELDS`, and `HEADER.FIELDS.NOT` part specifiers refer to the
    ///    [RFC-2822](https://tools.ietf.org/html/rfc2822) header of the message or of an
    ///    encapsulated [MIME-IMT](https://tools.ietf.org/html/rfc2046)
    ///    MESSAGE/[RFC822](https://tools.ietf.org/html/rfc822) message. `HEADER.FIELDS` and
    ///    `HEADER.FIELDS.NOT` are followed by a list of field-name (as defined in
    ///    [RFC-2822](https://tools.ietf.org/html/rfc2822)) names, and return a subset of the
    ///    header.  The subset returned by `HEADER.FIELDS` contains only those header fields with
    ///    a field-name that matches one of the names in the list; similarly, the subset returned
    ///    by `HEADER.FIELDS.NOT` contains only the header fields with a non-matching field-name.
    ///    The field-matching is case-insensitive but otherwise exact.  Subsetting does not
    ///    exclude the [RFC-2822](https://tools.ietf.org/html/rfc2822) delimiting blank line
    ///    between the header and the body; the blank line is included in all header fetches,
    ///    except in the case of a message which has no body and no blank line.
    ///
    ///    The `MIME` part specifier refers to the [MIME-IMB](https://tools.ietf.org/html/rfc2045)
    ///    header for this part.
    ///
    ///    The `TEXT` part specifier refers to the text body of the message,
    ///    omitting the [RFC-2822](https://tools.ietf.org/html/rfc2822) header.
    ///
    ///    [`Flag::Seen`] is implicitly set when `BODY` is fetched; if this causes the flags to
    ///    change, they will generally be included as part of the `FETCH` responses.
    ///  - `BODY.PEEK[<section>]`: An alternate form of `BODY[<section>]` that does not implicitly
    ///    set [`Flag::Seen`].
    ///  - `ENVELOPE`: The envelope structure of the message.  This is computed by the server by
    ///    parsing the [RFC-2822](https://tools.ietf.org/html/rfc2822) header into the component
    ///    parts, defaulting various fields as necessary.
    ///  - `RFC822`: Functionally equivalent to `BODY[]`.
    ///  - `RFC822.HEADER`: Functionally equivalent to `BODY.PEEK[HEADER]`.
    ///  - `RFC822.SIZE`: The [RFC-2822](https://tools.ietf.org/html/rfc2822) size of the message.
    ///  - `UID`: The unique identifier for the message.
    pub async fn fetch<S1, S2>(
        &mut self,
        sequence_set: S1,
        query: S2,
    ) -> Result<impl Stream<Item = Result<Fetch>> + '_ + Send>
    where
        S1: AsRef<str>,
        S2: AsRef<str>,
    {
        let id = self
            .run_command(&format!(
                "FETCH {} {}",
                sequence_set.as_ref(),
                query.as_ref()
            ))
            .await?;
        let res = parse_fetches(
            &mut self.conn.stream,
            self.unsolicited_responses_tx.clone(),
            id,
        );

        Ok(res)
    }

    /// Equivalent to [`Session::fetch`], except that all identifiers in `uid_set` are
    /// [`Uid`]s. See also the [`UID` command](https://tools.ietf.org/html/rfc3501#section-6.4.8).
    pub async fn uid_fetch<S1, S2>(
        &mut self,
        uid_set: S1,
        query: S2,
    ) -> Result<impl Stream<Item = Result<Fetch>> + '_ + Send + Unpin>
    where
        S1: AsRef<str>,
        S2: AsRef<str>,
    {
        let id = self
            .run_command(&format!(
                "UID FETCH {} {}",
                uid_set.as_ref(),
                query.as_ref()
            ))
            .await?;
        let res = parse_fetches(
            &mut self.conn.stream,
            self.unsolicited_responses_tx.clone(),
            id,
        );
        Ok(res)
    }

    /// Noop always succeeds, and it does nothing.
    pub async fn noop(&mut self) -> Result<()> {
        let id = self.run_command("NOOP").await?;
        parse_noop(
            &mut self.conn.stream,
            self.unsolicited_responses_tx.clone(),
            id,
        )
        .await?;
        Ok(())
    }

    /// Logout informs the server that the client is done with the connection.
    pub async fn logout(&mut self) -> Result<()> {
        self.run_command_and_check_ok("LOGOUT").await?;
        Ok(())
    }

    /// The [`CREATE` command](https://tools.ietf.org/html/rfc3501#section-6.3.3) creates a mailbox
    /// with the given name.  `Ok` is returned only if a new mailbox with that name has been
    /// created.  It is an error to attempt to create `INBOX` or a mailbox with a name that
    /// refers to an extant mailbox.  Any error in creation will return [`Error::No`].
    ///
    /// If the mailbox name is suffixed with the server's hierarchy separator character (as
    /// returned from the server by [`Session::list`]), this is a declaration that the client
    /// intends to create mailbox names under this name in the hierarchy.  Servers that do not
    /// require this declaration will ignore the declaration.  In any case, the name created is
    /// without the trailing hierarchy delimiter.
    ///
    /// If the server's hierarchy separator character appears elsewhere in the name, the server
    /// will generally create any superior hierarchical names that are needed for the `CREATE`
    /// command to be successfully completed.  In other words, an attempt to create `foo/bar/zap`
    /// on a server in which `/` is the hierarchy separator character will usually create `foo/`
    /// and `foo/bar/` if they do not already exist.
    ///
    /// If a new mailbox is created with the same name as a mailbox which was deleted, its unique
    /// identifiers will be greater than any unique identifiers used in the previous incarnation of
    /// the mailbox UNLESS the new incarnation has a different unique identifier validity value.
    /// See the description of the [`UID`
    /// command](https://tools.ietf.org/html/rfc3501#section-6.4.8) for more detail.
    pub async fn create<S: AsRef<str>>(&mut self, mailbox_name: S) -> Result<()> {
        self.run_command_and_check_ok(&format!("CREATE {}", validate_str(mailbox_name.as_ref())?))
            .await?;

        Ok(())
    }

    /// The [`DELETE` command](https://tools.ietf.org/html/rfc3501#section-6.3.4) permanently
    /// removes the mailbox with the given name.  `Ok` is returned only if the mailbox has been
    /// deleted.  It is an error to attempt to delete `INBOX` or a mailbox name that does not
    /// exist.
    ///
    /// The `DELETE` command will not remove inferior hierarchical names. For example, if a mailbox
    /// `foo` has an inferior `foo.bar` (assuming `.` is the hierarchy delimiter character),
    /// removing `foo` will not remove `foo.bar`.  It is an error to attempt to delete a name that
    /// has inferior hierarchical names and also has [`NameAttribute::NoSelect`].
    ///
    /// It is permitted to delete a name that has inferior hierarchical names and does not have
    /// [`NameAttribute::NoSelect`].  In this case, all messages in that mailbox are removed, and
    /// the name will acquire [`NameAttribute::NoSelect`].
    ///
    /// The value of the highest-used unique identifier of the deleted mailbox will be preserved so
    /// that a new mailbox created with the same name will not reuse the identifiers of the former
    /// incarnation, UNLESS the new incarnation has a different unique identifier validity value.
    /// See the description of the [`UID`
    /// command](https://tools.ietf.org/html/rfc3501#section-6.4.8) for more detail.
    pub async fn delete<S: AsRef<str>>(&mut self, mailbox_name: S) -> Result<()> {
        self.run_command_and_check_ok(&format!("DELETE {}", validate_str(mailbox_name.as_ref())?))
            .await?;

        Ok(())
    }

    /// The [`RENAME` command](https://tools.ietf.org/html/rfc3501#section-6.3.5) changes the name
    /// of a mailbox.  `Ok` is returned only if the mailbox has been renamed.  It is an error to
    /// attempt to rename from a mailbox name that does not exist or to a mailbox name that already
    /// exists.  Any error in renaming will return [`Error::No`].
    ///
    /// If the name has inferior hierarchical names, then the inferior hierarchical names will also
    /// be renamed.  For example, a rename of `foo` to `zap` will rename `foo/bar` (assuming `/` is
    /// the hierarchy delimiter character) to `zap/bar`.
    ///
    /// If the server's hierarchy separator character appears in the name, the server will
    /// generally create any superior hierarchical names that are needed for the `RENAME` command
    /// to complete successfully.  In other words, an attempt to rename `foo/bar/zap` to
    /// `baz/rag/zowie` on a server in which `/` is the hierarchy separator character will
    /// generally create `baz/` and `baz/rag/` if they do not already exist.
    ///
    /// The value of the highest-used unique identifier of the old mailbox name will be preserved
    /// so that a new mailbox created with the same name will not reuse the identifiers of the
    /// former incarnation, UNLESS the new incarnation has a different unique identifier validity
    /// value. See the description of the [`UID`
    /// command](https://tools.ietf.org/html/rfc3501#section-6.4.8) for more detail.
    ///
    /// Renaming `INBOX` is permitted, and has special behavior.  It moves all messages in `INBOX`
    /// to a new mailbox with the given name, leaving `INBOX` empty.  If the server implementation
    /// supports inferior hierarchical names of `INBOX`, these are unaffected by a rename of
    /// `INBOX`.
    pub async fn rename<S1: AsRef<str>, S2: AsRef<str>>(&mut self, from: S1, to: S2) -> Result<()> {
        self.run_command_and_check_ok(&format!(
            "RENAME {} {}",
            validate_str(from.as_ref())?,
            validate_str(to.as_ref())?
        ))
        .await?;

        Ok(())
    }

    /// The [`SUBSCRIBE` command](https://tools.ietf.org/html/rfc3501#section-6.3.6) adds the
    /// specified mailbox name to the server's set of "active" or "subscribed" mailboxes as
    /// returned by [`Session::lsub`].  This command returns `Ok` only if the subscription is
    /// successful.
    ///
    /// The server may validate the mailbox argument to `SUBSCRIBE` to verify that it exists.
    /// However, it will not unilaterally remove an existing mailbox name from the subscription
    /// list even if a mailbox by that name no longer exists.
    pub async fn subscribe<S: AsRef<str>>(&mut self, mailbox: S) -> Result<()> {
        self.run_command_and_check_ok(&format!("SUBSCRIBE {}", validate_str(mailbox.as_ref())?))
            .await?;
        Ok(())
    }

    /// The [`UNSUBSCRIBE` command](https://tools.ietf.org/html/rfc3501#section-6.3.7) removes the
    /// specified mailbox name from the server's set of "active" or "subscribed" mailboxes as
    /// returned by [`Session::lsub`].  This command returns `Ok` only if the unsubscription is
    /// successful.
    pub async fn unsubscribe<S: AsRef<str>>(&mut self, mailbox: S) -> Result<()> {
        self.run_command_and_check_ok(&format!("UNSUBSCRIBE {}", validate_str(mailbox.as_ref())?))
            .await?;
        Ok(())
    }

    /// The [`CAPABILITY` command](https://tools.ietf.org/html/rfc3501#section-6.1.1) requests a
    /// listing of capabilities that the server supports.  The server will include "IMAP4rev1" as
    /// one of the listed capabilities. See [`Capabilities`] for further details.
    pub async fn capabilities(&mut self) -> Result<Capabilities> {
        let id = self.run_command("CAPABILITY").await?;
        let c = parse_capabilities(
            &mut self.conn.stream,
            self.unsolicited_responses_tx.clone(),
            id,
        )
        .await?;
        Ok(c)
    }

    /// The [`EXPUNGE` command](https://tools.ietf.org/html/rfc3501#section-6.4.3) permanently
    /// removes all messages that have [`Flag::Deleted`] set from the currently selected mailbox.
    /// The message sequence number of each message that is removed is returned.
    pub async fn expunge(&mut self) -> Result<impl Stream<Item = Result<Seq>> + '_ + Send> {
        let id = self.run_command("EXPUNGE").await?;
        let res = parse_expunge(
            &mut self.conn.stream,
            self.unsolicited_responses_tx.clone(),
            id,
        );
        Ok(res)
    }

    /// The [`UID EXPUNGE` command](https://tools.ietf.org/html/rfc4315#section-2.1) permanently
    /// removes all messages that both have [`Flag::Deleted`] set and have a [`Uid`] that is
    /// included in the specified sequence set from the currently selected mailbox.  If a message
    /// either does not have [`Flag::Deleted`] set or has a [`Uid`] that is not included in the
    /// specified sequence set, it is not affected.
    ///
    /// This command is particularly useful for disconnected use clients. By using `uid_expunge`
    /// instead of [`Self::expunge`] when resynchronizing with the server, the client can ensure that it
    /// does not inadvertantly remove any messages that have been marked as [`Flag::Deleted`] by
    /// other clients between the time that the client was last connected and the time the client
    /// resynchronizes.
    ///
    /// This command requires that the server supports [RFC
    /// 4315](https://tools.ietf.org/html/rfc4315) as indicated by the `UIDPLUS` capability (see
    /// [`Session::capabilities`]). If the server does not support the `UIDPLUS` capability, the
    /// client should fall back to using [`Session::store`] to temporarily remove [`Flag::Deleted`]
    /// from messages it does not want to remove, then invoking [`Session::expunge`].  Finally, the
    /// client should use [`Session::store`] to restore [`Flag::Deleted`] on the messages in which
    /// it was temporarily removed.
    ///
    /// Alternatively, the client may fall back to using just [`Session::expunge`], risking the
    /// unintended removal of some messages.
    pub async fn uid_expunge<S: AsRef<str>>(
        &mut self,
        uid_set: S,
    ) -> Result<impl Stream<Item = Result<Uid>> + '_ + Send> {
        let id = self
            .run_command(&format!("UID EXPUNGE {}", uid_set.as_ref()))
            .await?;
        let res = parse_expunge(
            &mut self.conn.stream,
            self.unsolicited_responses_tx.clone(),
            id,
        );
        Ok(res)
    }

    /// The [`CHECK` command](https://tools.ietf.org/html/rfc3501#section-6.4.1) requests a
    /// checkpoint of the currently selected mailbox.  A checkpoint refers to any
    /// implementation-dependent housekeeping associated with the mailbox (e.g., resolving the
    /// server's in-memory state of the mailbox with the state on its disk) that is not normally
    /// executed as part of each command.  A checkpoint MAY take a non-instantaneous amount of real
    /// time to complete.  If a server implementation has no such housekeeping considerations,
    /// [`Session::check`] is equivalent to [`Session::noop`].
    ///
    /// There is no guarantee that an `EXISTS` untagged response will happen as a result of
    /// `CHECK`.  [`Session::noop`] SHOULD be used for new message polling.
    pub async fn check(&mut self) -> Result<()> {
        self.run_command_and_check_ok("CHECK").await?;
        Ok(())
    }

    /// The [`CLOSE` command](https://tools.ietf.org/html/rfc3501#section-6.4.2) permanently
    /// removes all messages that have [`Flag::Deleted`] set from the currently selected mailbox,
    /// and returns to the authenticated state from the selected state.  No `EXPUNGE` responses are
    /// sent.
    ///
    /// No messages are removed, and no error is given, if the mailbox is selected by
    /// [`Session::examine`] or is otherwise selected read-only.
    ///
    /// Even if a mailbox is selected, [`Session::select`], [`Session::examine`], or
    /// [`Session::logout`] command MAY be issued without previously invoking [`Session::close`].
    /// [`Session::select`], [`Session::examine`], and [`Session::logout`] implicitly close the
    /// currently selected mailbox without doing an expunge.  However, when many messages are
    /// deleted, a `CLOSE-LOGOUT` or `CLOSE-SELECT` sequence is considerably faster than an
    /// `EXPUNGE-LOGOUT` or `EXPUNGE-SELECT` because no `EXPUNGE` responses (which the client would
    /// probably ignore) are sent.
    pub async fn close(&mut self) -> Result<()> {
        self.run_command_and_check_ok("CLOSE").await?;
        Ok(())
    }

    /// The [`STORE` command](https://tools.ietf.org/html/rfc3501#section-6.4.6) alters data
    /// associated with a message in the mailbox.  Normally, `STORE` will return the updated value
    /// of the data with an untagged FETCH response.  A suffix of `.SILENT` in `query` prevents the
    /// untagged `FETCH`, and the server assumes that the client has determined the updated value
    /// itself or does not care about the updated value.
    ///
    /// The currently defined data items that can be stored are:
    ///
    ///  - `FLAGS <flag list>`:
    ///
    ///    Replace the flags for the message (other than [`Flag::Recent`]) with the argument.  The
    ///    new value of the flags is returned as if a `FETCH` of those flags was done.
    ///
    ///  - `FLAGS.SILENT <flag list>`: Equivalent to `FLAGS`, but without returning a new value.
    ///
    ///  - `+FLAGS <flag list>`
    ///
    ///    Add the argument to the flags for the message.  The new value of the flags is returned
    ///    as if a `FETCH` of those flags was done.
    ///  - `+FLAGS.SILENT <flag list>`: Equivalent to `+FLAGS`, but without returning a new value.
    ///
    ///  - `-FLAGS <flag list>`
    ///
    ///    Remove the argument from the flags for the message.  The new value of the flags is
    ///    returned as if a `FETCH` of those flags was done.
    ///
    ///  - `-FLAGS.SILENT <flag list>`: Equivalent to `-FLAGS`, but without returning a new value.
    ///
    /// In all cases, `<flag list>` is a space-separated list enclosed in parentheses.
    ///
    /// # Examples
    ///
    /// Delete a message:
    ///
    /// ```no_run
    /// use async_imap::{types::Seq, Session, error::Result};
    /// #[cfg(feature = "runtime-async-std")]
    /// use async_std::net::TcpStream;
    /// #[cfg(feature = "runtime-tokio")]
    /// use tokio::net::TcpStream;
    /// use futures_util::TryStreamExt;
    ///
    /// async fn delete(seq: Seq, s: &mut Session<TcpStream>) -> Result<()> {
    ///     let updates_stream = s.store(format!("{}", seq), "+FLAGS (\\Deleted)").await?;
    ///     let _updates: Vec<_> = updates_stream.try_collect().await?;
    ///     s.expunge().await?;
    ///     Ok(())
    /// }
    /// ```
    pub async fn store<S1, S2>(
        &mut self,
        sequence_set: S1,
        query: S2,
    ) -> Result<impl Stream<Item = Result<Fetch>> + '_ + Send>
    where
        S1: AsRef<str>,
        S2: AsRef<str>,
    {
        let id = self
            .run_command(&format!(
                "STORE {} {}",
                sequence_set.as_ref(),
                query.as_ref()
            ))
            .await?;
        let res = parse_fetches(
            &mut self.conn.stream,
            self.unsolicited_responses_tx.clone(),
            id,
        );
        Ok(res)
    }

    /// Equivalent to [`Session::store`], except that all identifiers in `sequence_set` are
    /// [`Uid`]s. See also the [`UID` command](https://tools.ietf.org/html/rfc3501#section-6.4.8).
    pub async fn uid_store<S1, S2>(
        &mut self,
        uid_set: S1,
        query: S2,
    ) -> Result<impl Stream<Item = Result<Fetch>> + '_ + Send>
    where
        S1: AsRef<str>,
        S2: AsRef<str>,
    {
        let id = self
            .run_command(&format!(
                "UID STORE {} {}",
                uid_set.as_ref(),
                query.as_ref()
            ))
            .await?;
        let res = parse_fetches(
            &mut self.conn.stream,
            self.unsolicited_responses_tx.clone(),
            id,
        );
        Ok(res)
    }

    /// The [`COPY` command](https://tools.ietf.org/html/rfc3501#section-6.4.7) copies the
    /// specified message(s) to the end of the specified destination mailbox.  The flags and
    /// internal date of the message(s) will generally be preserved, and [`Flag::Recent`] will
    /// generally be set, in the copy.
    ///
    /// If the `COPY` command is unsuccessful for any reason, the server restores the destination
    /// mailbox to its state before the `COPY` attempt.
    pub async fn copy<S1: AsRef<str>, S2: AsRef<str>>(
        &mut self,
        sequence_set: S1,
        mailbox_name: S2,
    ) -> Result<()> {
        self.run_command_and_check_ok(&format!(
            "COPY {} {}",
            sequence_set.as_ref(),
            validate_str(mailbox_name.as_ref())?
        ))
        .await?;

        Ok(())
    }

    /// Equivalent to [`Session::copy`], except that all identifiers in `sequence_set` are
    /// [`Uid`]s. See also the [`UID` command](https://tools.ietf.org/html/rfc3501#section-6.4.8).
    pub async fn uid_copy<S1: AsRef<str>, S2: AsRef<str>>(
        &mut self,
        uid_set: S1,
        mailbox_name: S2,
    ) -> Result<()> {
        self.run_command_and_check_ok(&format!(
            "UID COPY {} {}",
            uid_set.as_ref(),
            validate_str(mailbox_name.as_ref())?
        ))
        .await?;

        Ok(())
    }

    /// The [`MOVE` command](https://tools.ietf.org/html/rfc6851#section-3.1) takes two
    /// arguments: a sequence set and a named mailbox. Each message included in the set is moved,
    /// rather than copied, from the selected (source) mailbox to the named (target) mailbox.
    ///
    /// This means that a new message is created in the target mailbox with a
    /// new [`Uid`], the original message is removed from the source mailbox, and
    /// it appears to the client as a single action.  This has the same
    /// effect for each message as this sequence:
    ///
    ///   1. COPY
    ///   2. STORE +FLAGS.SILENT \DELETED
    ///   3. EXPUNGE
    ///
    /// This command requires that the server supports [RFC
    /// 6851](https://tools.ietf.org/html/rfc6851) as indicated by the `MOVE` capability (see
    /// [`Session::capabilities`]).
    ///
    /// Although the effect of the `MOVE` is the same as the preceding steps, the semantics are not
    /// identical: The intermediate states produced by those steps do not occur, and the response
    /// codes are different.  In particular, though the `COPY` and `EXPUNGE` response codes will be
    /// returned, response codes for a `store` will not be generated and [`Flag::Deleted`] will not
    /// be set for any message.
    ///
    /// Because a `MOVE` applies to a set of messages, it might fail partway through the set.
    /// Regardless of whether the command is successful in moving the entire set, each individual
    /// message will either be moved or unaffected.  The server will leave each message in a state
    /// where it is in at least one of the source or target mailboxes (no message can be lost or
    /// orphaned).  The server will generally not leave any message in both mailboxes (it would be
    /// bad for a partial failure to result in a bunch of duplicate messages).  This is true even
    /// if the server returns with [`Error::No`].
    pub async fn mv<S1: AsRef<str>, S2: AsRef<str>>(
        &mut self,
        sequence_set: S1,
        mailbox_name: S2,
    ) -> Result<()> {
        self.run_command_and_check_ok(&format!(
            "MOVE {} {}",
            sequence_set.as_ref(),
            validate_str(mailbox_name.as_ref())?
        ))
        .await?;

        Ok(())
    }

    /// Equivalent to [`Session::copy`], except that all identifiers in `sequence_set` are
    /// [`Uid`]s. See also the [`UID` command](https://tools.ietf.org/html/rfc3501#section-6.4.8)
    /// and the [semantics of `MOVE` and `UID
    /// MOVE`](https://tools.ietf.org/html/rfc6851#section-3.3).
    pub async fn uid_mv<S1: AsRef<str>, S2: AsRef<str>>(
        &mut self,
        uid_set: S1,
        mailbox_name: S2,
    ) -> Result<()> {
        self.run_command_and_check_ok(&format!(
            "UID MOVE {} {}",
            uid_set.as_ref(),
            validate_str(mailbox_name.as_ref())?
        ))
        .await?;

        Ok(())
    }

    /// The [`LIST` command](https://tools.ietf.org/html/rfc3501#section-6.3.8) returns a subset of
    /// names from the complete set of all names available to the client.  It returns the name
    /// attributes, hierarchy delimiter, and name of each such name; see [`Name`] for more detail.
    ///
    /// If `reference_name` is `None` (or `""`), the currently selected mailbox is used.
    /// The returned mailbox names must match the supplied `mailbox_pattern`.  A non-empty
    /// reference name argument is the name of a mailbox or a level of mailbox hierarchy, and
    /// indicates the context in which the mailbox name is interpreted.
    ///
    /// If `mailbox_pattern` is `None` (or `""`), it is a special request to return the hierarchy
    /// delimiter and the root name of the name given in the reference.  The value returned as the
    /// root MAY be the empty string if the reference is non-rooted or is an empty string.  In all
    /// cases, a hierarchy delimiter (or `NIL` if there is no hierarchy) is returned.  This permits
    /// a client to get the hierarchy delimiter (or find out that the mailbox names are flat) even
    /// when no mailboxes by that name currently exist.
    ///
    /// The reference and mailbox name arguments are interpreted into a canonical form that
    /// represents an unambiguous left-to-right hierarchy.  The returned mailbox names will be in
    /// the interpreted form.
    ///
    /// The character `*` is a wildcard, and matches zero or more characters at this position.  The
    /// character `%` is similar to `*`, but it does not match a hierarchy delimiter.  If the `%`
    /// wildcard is the last character of a mailbox name argument, matching levels of hierarchy are
    /// also returned.  If these levels of hierarchy are not also selectable mailboxes, they are
    /// returned with [`NameAttribute::NoSelect`].
    ///
    /// The special name `INBOX` is included if `INBOX` is supported by this server for this user
    /// and if the uppercase string `INBOX` matches the interpreted reference and mailbox name
    /// arguments with wildcards.  The criteria for omitting `INBOX` is whether `SELECT INBOX` will
    /// return failure; it is not relevant whether the user's real `INBOX` resides on this or some
    /// other server.
    pub async fn list(
        &mut self,
        reference_name: Option<&str>,
        mailbox_pattern: Option<&str>,
    ) -> Result<impl Stream<Item = Result<Name>> + '_ + Send + use<'_, T>> {
        let id = self
            .run_command(&format!(
                "LIST {} {}",
                validate_str(reference_name.unwrap_or(""))?,
                mailbox_pattern.unwrap_or("\"\"")
            ))
            .await?;
        let names = parse_names(
            &mut self.conn.stream,
            self.unsolicited_responses_tx.clone(),
            id,
        );

        Ok(names)
    }

    /// The [`LSUB` command](https://tools.ietf.org/html/rfc3501#section-6.3.9) returns a subset of
    /// names from the set of names that the user has declared as being "active" or "subscribed".
    /// The arguments to this method the same as for [`Session::list`].
    ///
    /// The returned [`Name`]s MAY contain different mailbox flags from response to
    /// [`Session::list`].  If this should happen, the flags returned by [`Session::list`] are
    /// considered more authoritative.
    ///
    /// A special situation occurs when invoking `lsub` with the `%` wildcard. Consider what
    /// happens if `foo/bar` (with a hierarchy delimiter of `/`) is subscribed but `foo` is not.  A
    /// `%` wildcard to `lsub` must return `foo`, not `foo/bar`, and it will be flagged with
    /// [`NameAttribute::NoSelect`].
    ///
    /// The server will not unilaterally remove an existing mailbox name from the subscription list
    /// even if a mailbox by that name no longer exists.
    pub async fn lsub(
        &mut self,
        reference_name: Option<&str>,
        mailbox_pattern: Option<&str>,
    ) -> Result<impl Stream<Item = Result<Name>> + '_ + Send + use<'_, T>> {
        let id = self
            .run_command(&format!(
                "LSUB {} {}",
                validate_str(reference_name.unwrap_or(""))?,
                validate_str(mailbox_pattern.unwrap_or(""))?
            ))
            .await?;
        let names = parse_names(
            &mut self.conn.stream,
            self.unsolicited_responses_tx.clone(),
            id,
        );

        Ok(names)
    }

    /// The [`STATUS` command](https://tools.ietf.org/html/rfc3501#section-6.3.10) requests the
    /// status of the indicated mailbox. It does not change the currently selected mailbox, nor
    /// does it affect the state of any messages in the queried mailbox (in particular, `status`
    /// will not cause messages to lose [`Flag::Recent`]).
    ///
    /// `status` provides an alternative to opening a second [`Session`] and using
    /// [`Session::examine`] on a mailbox to query that mailbox's status without deselecting the
    /// current mailbox in the first `Session`.
    ///
    /// Unlike [`Session::list`], `status` is not guaranteed to be fast in its response.  Under
    /// certain circumstances, it can be quite slow.  In some implementations, the server is
    /// obliged to open the mailbox read-only internally to obtain certain status information.
    /// Also unlike [`Session::list`], `status` does not accept wildcards.
    ///
    /// > Note: `status` is intended to access the status of mailboxes other than the currently
    /// > selected mailbox.  Because `status` can cause the mailbox to be opened internally, and
    /// > because this information is available by other means on the selected mailbox, `status`
    /// > SHOULD NOT be used on the currently selected mailbox.
    ///
    /// The STATUS command MUST NOT be used as a "check for new messages in the selected mailbox"
    /// operation (refer to sections [7](https://tools.ietf.org/html/rfc3501#section-7),
    /// [7.3.1](https://tools.ietf.org/html/rfc3501#section-7.3.1), and
    /// [7.3.2](https://tools.ietf.org/html/rfc3501#section-7.3.2) for more information about the
    /// proper method for new message checking).
    ///
    /// The currently defined status data items that can be requested are:
    ///
    ///  - `MESSAGES`: The number of messages in the mailbox.
    ///  - `RECENT`: The number of messages with [`Flag::Recent`] set.
    ///  - `UIDNEXT`: The next [`Uid`] of the mailbox.
    ///  - `UIDVALIDITY`: The unique identifier validity value of the mailbox (see [`Uid`]).
    ///  - `UNSEEN`: The number of messages which do not have [`Flag::Seen`] set.
    ///
    /// `data_items` is a space-separated list enclosed in parentheses.
    pub async fn status<S1: AsRef<str>, S2: AsRef<str>>(
        &mut self,
        mailbox_name: S1,
        data_items: S2,
    ) -> Result<Mailbox> {
        let id = self
            .run_command(&format!(
                "STATUS {} {}",
                validate_str(mailbox_name.as_ref())?,
                data_items.as_ref()
            ))
            .await?;
        let mbox = parse_status(
            &mut self.conn.stream,
            mailbox_name.as_ref(),
            self.unsolicited_responses_tx.clone(),
            id,
        )
        .await?;
        Ok(mbox)
    }

    /// This method returns a handle that lets you use the [`IDLE`
    /// command](https://tools.ietf.org/html/rfc2177#section-3) to listen for changes to the
    /// currently selected mailbox.
    ///
    /// It's often more desirable to have the server transmit updates to the client in real time.
    /// This allows a user to see new mail immediately.  It also helps some real-time applications
    /// based on IMAP, which might otherwise need to poll extremely often (such as every few
    /// seconds).  While the spec actually does allow a server to push `EXISTS` responses
    /// aysynchronously, a client can't expect this behaviour and must poll.  This method provides
    /// you with such a mechanism.
    ///
    /// `idle` may be used with any server that returns `IDLE` as one of the supported capabilities
    /// (see [`Session::capabilities`]). If the server does not advertise the `IDLE` capability,
    /// the client MUST NOT use `idle` and must instead poll for mailbox updates.  In particular,
    /// the client MUST continue to be able to accept unsolicited untagged responses to ANY
    /// command, as specified in the base IMAP specification.
    ///
    /// See [`extensions::idle::Handle`] for details.
    pub fn idle(self) -> extensions::idle::Handle<T> {
        extensions::idle::Handle::new(self)
    }

    /// The [`APPEND` command](https://tools.ietf.org/html/rfc3501#section-6.3.11) appends
    /// `content` as a new message to the end of the specified destination `mailbox`.  This
    /// argument SHOULD be in the format of an [RFC-2822](https://tools.ietf.org/html/rfc2822)
    /// message.
    ///
    /// > Note: There MAY be exceptions, e.g., draft messages, in which required RFC-2822 header
    /// > lines are omitted in the message literal argument to `append`.  The full implications of
    /// > doing so MUST be understood and carefully weighed.
    ///
    /// If the append is unsuccessful for any reason, the mailbox is restored to its state before
    /// the append attempt; no partial appending will happen.
    ///
    /// If the destination `mailbox` does not exist, the server returns an error, and does not
    /// automatically create the mailbox.
    ///
    /// If the mailbox is currently selected, the normal new message actions will generally occur.
    /// Specifically, the server will generally notify the client immediately via an untagged
    /// `EXISTS` response.  If the server does not do so, the client MAY issue a `NOOP` command (or
    /// failing that, a `CHECK` command) after one or more `APPEND` commands.
    pub async fn append(
        &mut self,
        mailbox: impl AsRef<str>,
        flags: Option<&str>,
        internaldate: Option<&str>,
        content: impl AsRef<[u8]>,
    ) -> Result<()> {
        let content = content.as_ref();
        let id = self
            .run_command(&format!(
                "APPEND {}{}{}{}{} {{{}}}",
                validate_str(mailbox.as_ref())?,
                if flags.is_some() { " " } else { "" },
                flags.unwrap_or(""),
                if internaldate.is_some() { " " } else { "" },
                internaldate.unwrap_or(""),
                content.len()
            ))
            .await?;

        loop {
            let Some(res) = self.read_response().await? else {
                return Err(Error::Append);
            };
            match res.parsed() {
                Response::Continue { .. } => break,
                // A tagged result (even OK) before the literal, or a closing
                // connection, cannot authorize sending the message bytes.
                Response::Done { .. }
                | Response::Data {
                    status: imap_proto::Status::Bye,
                    ..
                } => {
                    return Err(Error::Append);
                }
                _ => handle_unilateral(res, self.unsolicited_responses_tx.clone()),
            }
        }

        self.stream.as_mut().write_all(content).await?;
        self.stream.as_mut().write_all(b"\r\n").await?;
        self.stream.flush().await?;
        self.conn
            .check_done_ok(&id, Some(self.unsolicited_responses_tx.clone()))
            .await?;
        Ok(())
    }

    /// The [`SEARCH` command](https://tools.ietf.org/html/rfc3501#section-6.4.4) searches the
    /// mailbox for messages that match the given `query`.  `query` consist of one or more search
    /// keys separated by spaces.  The response from the server contains a listing of [`Seq`]s
    /// corresponding to those messages that match the searching criteria.
    ///
    /// When multiple search keys are specified, the result is the intersection of all the messages
    /// that match those keys.  Or, in other words, only messages that match *all* the keys. For
    /// example, the criteria
    ///
    /// ```text
    /// DELETED FROM "SMITH" SINCE 1-Feb-1994
    /// ```
    ///
    /// refers to all deleted messages from Smith that were placed in the mailbox since February 1,
    /// 1994.  A search key can also be a parenthesized list of one or more search keys (e.g., for
    /// use with the `OR` and `NOT` keys).
    ///
    /// In all search keys that use strings, a message matches the key if the string is a substring
    /// of the field.  The matching is case-insensitive.
    ///
    /// Below is a selection of common search keys.  The full list can be found in the
    /// specification of the [`SEARCH command`](https://tools.ietf.org/html/rfc3501#section-6.4.4).
    ///
    ///  - `NEW`: Messages that have [`Flag::Recent`] set but not [`Flag::Seen`]. This is functionally equivalent to `(RECENT UNSEEN)`.
    ///  - `OLD`: Messages that do not have [`Flag::Recent`] set.  This is functionally equivalent to `NOT RECENT` (as opposed to `NOT NEW`).
    ///  - `RECENT`: Messages that have [`Flag::Recent`] set.
    ///  - `ANSWERED`: Messages with [`Flag::Answered`] set.
    ///  - `DELETED`: Messages with [`Flag::Deleted`] set.
    ///  - `DRAFT`: Messages with [`Flag::Draft`] set.
    ///  - `FLAGGED`: Messages with [`Flag::Flagged`] set.
    ///  - `SEEN`: Messages that have [`Flag::Seen`] set.
    ///  - `<sequence set>`: Messages with message sequence numbers corresponding to the specified message sequence number set.
    ///  - `UID <sequence set>`: Messages with [`Uid`] corresponding to the specified unique identifier set.  Sequence set ranges are permitted.
    ///
    ///  - `SUBJECT <string>`: Messages that contain the specified string in the envelope structure's `SUBJECT` field.
    ///  - `BODY <string>`: Messages that contain the specified string in the body of the message.
    ///  - `FROM <string>`: Messages that contain the specified string in the envelope structure's `FROM` field.
    ///  - `TO <string>`: Messages that contain the specified string in the envelope structure's `TO` field.
    ///
    ///  - `NOT <search-key>`: Messages that do not match the specified search key.
    ///  - `OR <search-key1> <search-key2>`: Messages that match either search key.
    ///
    ///  - `BEFORE <date>`: Messages whose internal date (disregarding time and timezone) is earlier than the specified date.
    ///  - `SINCE <date>`: Messages whose internal date (disregarding time and timezone) is within or later than the specified date.
    pub async fn search<S: AsRef<str>>(&mut self, query: S) -> Result<HashSet<Seq>> {
        let id = self
            .run_command(&format!("SEARCH {}", query.as_ref()))
            .await?;
        let seqs = parse_ids(
            &mut self.conn.stream,
            self.unsolicited_responses_tx.clone(),
            id,
        )
        .await?;

        Ok(seqs)
    }

    /// Equivalent to [`Session::search`], except that the returned identifiers
    /// are [`Uid`] instead of [`Seq`]. See also the [`UID`
    /// command](https://tools.ietf.org/html/rfc3501#section-6.4.8).
    pub async fn uid_search<S: AsRef<str>>(&mut self, query: S) -> Result<HashSet<Uid>> {
        let id = self
            .run_command(&format!("UID SEARCH {}", query.as_ref()))
            .await?;
        let uids = parse_ids(
            &mut self.conn.stream,
            self.unsolicited_responses_tx.clone(),
            id,
        )
        .await?;

        Ok(uids)
    }

    /// Run a UID SEARCH with at most 200 UID entries and 128 KiB across all
    /// responses. A result requires one SEARCH response and matching tagged OK.
    /// Supply a complete query without synchronizing literals.
    pub async fn uid_search_bounded<S: AsRef<str>>(
        &mut self,
        query: S,
        max_uids: usize,
    ) -> Result<Vec<Uid>> {
        let tag = self
            .run_command_with_limits(
                format!("UID SEARCH {}", query.as_ref()),
                CommandReadLimits::search(CommandReadLimits::MAX_SEARCH_RESPONSE_BYTES, max_uids),
            )
            .await?;
        let result = self.collect_bounded_search(&tag).await;
        if result.is_err() {
            self.conn.stream.close();
        }
        result
    }

    async fn collect_bounded_search(&mut self, tag: &RequestId) -> Result<Vec<Uid>> {
        let mut found = None;
        loop {
            let response = self.read_response().await?.ok_or(Error::ConnectionLost)?;
            match response.parsed() {
                Response::MailboxData(MailboxDatum::Search(uids)) => {
                    if uids
                        .iter()
                        .enumerate()
                        .any(|(index, uid)| uids[..index].contains(uid))
                    {
                        return Err(io::Error::other("Duplicate IMAP SEARCH UID").into());
                    }
                    found = Some(uids.clone());
                }
                Response::Done {
                    tag: received,
                    status,
                    ..
                } if received == tag => {
                    return match status {
                        Status::Ok => {
                            found.ok_or_else(|| io::Error::other("Missing IMAP SEARCH data").into())
                        }
                        Status::No => Err(Error::No("UID SEARCH rejected".into())),
                        Status::Bad => Err(Error::Bad("UID SEARCH rejected".into())),
                        _ => Err(io::Error::other("Invalid IMAP SEARCH completion").into()),
                    };
                }
                Response::Done { .. } => {
                    return Err(io::Error::other("Unexpected IMAP SEARCH command tag").into());
                }
                Response::MailboxData(MailboxDatum::Exists(_) | MailboxDatum::Recent(_))
                | Response::Expunge(_) => {
                    handle_unilateral(response, self.unsolicited_responses_tx.clone());
                }
                // Other legal notifications are consumed within the command
                // without retaining their parser backing buffers.
                _ => {}
            }
        }
    }

    /// The [`GETQUOTA` command](https://tools.ietf.org/html/rfc2087#section-4.2)
    pub async fn get_quota(&mut self, quota_root: &str) -> Result<Quota> {
        let id = self
            .run_command(format!("GETQUOTA {}", validate_str(quota_root)?))
            .await?;
        let c = parse_get_quota(
            &mut self.conn.stream,
            self.unsolicited_responses_tx.clone(),
            id,
        )
        .await?;
        Ok(c)
    }

    /// The [`GETQUOTAROOT` command](https://tools.ietf.org/html/rfc2087#section-4.3)
    pub async fn get_quota_root(
        &mut self,
        mailbox_name: &str,
    ) -> Result<(Vec<QuotaRoot>, Vec<Quota>)> {
        let id = self
            .run_command(format!("GETQUOTAROOT {}", validate_str(mailbox_name)?))
            .await?;
        let c = parse_get_quota_root(
            &mut self.conn.stream,
            self.unsolicited_responses_tx.clone(),
            id,
        )
        .await?;
        Ok(c)
    }

    /// The [`GETMETADATA` command](https://datatracker.ietf.org/doc/html/rfc5464.html#section-4.2)
    pub async fn get_metadata(
        &mut self,
        mailbox_name: &str,
        options: &str,
        entry_specifier: &str,
    ) -> Result<Vec<Metadata>> {
        let options = if options.is_empty() {
            String::new()
        } else {
            format!(" {options}")
        };
        let id = self
            .run_command(format!(
                "GETMETADATA {} {}{}",
                validate_str(mailbox_name)?,
                options,
                entry_specifier
            ))
            .await?;
        let metadata = parse_metadata(
            &mut self.conn.stream,
            mailbox_name,
            self.unsolicited_responses_tx.clone(),
            id,
        )
        .await?;
        Ok(metadata)
    }

    /// The [`ID` command](https://datatracker.ietf.org/doc/html/rfc2971)
    ///
    /// `identification` is an iterable sequence of pairs such as `("name", Some("MyMailClient"))`.
    pub async fn id(
        &mut self,
        identification: impl IntoIterator<Item = (&str, Option<&str>)>,
    ) -> Result<Option<HashMap<String, String>>> {
        let id = self
            .run_command(format!("ID ({})", format_identification(identification)))
            .await?;
        let server_identification = parse_id(
            &mut self.conn.stream,
            self.unsolicited_responses_tx.clone(),
            id,
        )
        .await?;
        Ok(server_identification)
    }

    /// Similar to `id`, but don't identify ourselves.
    ///
    /// Sends `ID NIL` command and returns server response.
    pub async fn id_nil(&mut self) -> Result<Option<HashMap<String, String>>> {
        let id = self.run_command("ID NIL").await?;
        let server_identification = parse_id(
            &mut self.conn.stream,
            self.unsolicited_responses_tx.clone(),
            id,
        )
        .await?;
        Ok(server_identification)
    }

    // these are only here because they are public interface, the rest is in `Connection`
    /// Runs a command and checks if it returns OK.
    pub async fn run_command_and_check_ok<S: AsRef<str>>(&mut self, command: S) -> Result<()> {
        self.conn
            .run_command_and_check_ok(
                command.as_ref(),
                Some(self.unsolicited_responses_tx.clone()),
            )
            .await?;

        Ok(())
    }

    /// Runs any command passed to it.
    pub async fn run_command<S: AsRef<str>>(&mut self, command: S) -> Result<RequestId> {
        let id = self.conn.run_command(command.as_ref()).await?;

        Ok(id)
    }

    /// Run one command with response limits until its matching completion.
    /// Consume every response through that completion before issuing another command.
    pub async fn run_command_with_limits<S: AsRef<str>>(
        &mut self,
        command: S,
        limits: CommandReadLimits,
    ) -> Result<RequestId> {
        let id = self.conn.request_ids.next().unwrap();
        self.conn.stream.set_command_limits(id.clone(), limits)?;
        self.conn
            .stream
            .encode(Request(
                Some(id.clone()),
                command.as_ref().as_bytes().into(),
            ))
            .await?;
        self.conn.stream.flush().await?;
        Ok(id)
    }

    /// Prevent further reads and writes after the consumer rejects bounded
    /// command evidence. Drop the session before opening a replacement.
    pub fn abort_bounded_command(&mut self) {
        self.conn.stream.close();
    }

    /// Runs an arbitrary command, without adding a tag to it.
    pub async fn run_command_untagged<S: AsRef<str>>(&mut self, command: S) -> Result<()> {
        self.conn.run_command_untagged(command.as_ref()).await?;

        Ok(())
    }

    /// Read the next response on the connection.
    pub async fn read_response(&mut self) -> io::Result<Option<ResponseData>> {
        self.conn.read_response().await
    }
}

impl<T: Read + Write + Unpin + fmt::Debug> Connection<T> {
    unsafe_pinned!(stream: ImapStream<T>);

    /// Gets a reference to the underlying stream.
    pub fn get_ref(&self) -> &T {
        self.stream.get_ref()
    }

    /// Gets a mutable reference to the underlying stream.
    pub fn get_mut(&mut self) -> &mut T {
        self.stream.get_mut()
    }

    /// Convert this connection into the raw underlying stream.
    pub fn into_inner(self) -> T {
        let Self { stream, .. } = self;
        stream.into_inner()
    }

    /// Read the next response on the connection.
    pub async fn read_response(&mut self) -> io::Result<Option<ResponseData>> {
        self.stream.try_next().await
    }

    pub(crate) async fn run_command_untagged(&mut self, command: &str) -> Result<()> {
        self.stream
            .encode(Request(None, command.as_bytes().into()))
            .await?;
        self.stream.flush().await?;
        Ok(())
    }

    pub(crate) async fn run_command(&mut self, command: &str) -> Result<RequestId> {
        let request_id = self.request_ids.next().unwrap(); // safe: never returns Err
        self.stream
            .encode(Request(Some(request_id.clone()), command.as_bytes().into()))
            .await?;
        self.stream.flush().await?;
        Ok(request_id)
    }

    /// Execute a command and check that the next response is a matching done.
    pub async fn run_command_and_check_ok(
        &mut self,
        command: &str,
        unsolicited: Option<channel::Sender<UnsolicitedResponse>>,
    ) -> Result<()> {
        let id = self.run_command(command).await?;
        self.check_done_ok(&id, unsolicited).await?;

        Ok(())
    }

    pub(crate) async fn check_done_ok(
        &mut self,
        id: &RequestId,
        unsolicited: Option<channel::Sender<UnsolicitedResponse>>,
    ) -> Result<()> {
        match self.stream.try_next().await? {
            Some(first_res) => self.check_done_ok_from(id, unsolicited, first_res).await,
            _ => Err(Error::ConnectionLost),
        }
    }

    pub(crate) async fn check_done_ok_from(
        &mut self,
        id: &RequestId,
        unsolicited: Option<channel::Sender<UnsolicitedResponse>>,
        mut response: ResponseData,
    ) -> Result<()> {
        loop {
            if let Response::Done {
                tag,
                status,
                outcome,
            } = response.parsed()
            {
                self.check_status_ok(
                    status,
                    outcome.code.as_ref(),
                    outcome.information.as_deref(),
                )?;

                if tag == id {
                    return Ok(());
                }
            }

            if let Some(unsolicited) = unsolicited.clone() {
                handle_unilateral(response, unsolicited);
            }

            let Some(res) = self.stream.try_next().await? else {
                return Err(Error::ConnectionLost);
            };
            response = res;
        }
    }

    pub(crate) fn check_status_ok(
        &self,
        status: &imap_proto::Status,
        code: Option<&imap_proto::ResponseCode<'_>>,
        information: Option<&str>,
    ) -> Result<()> {
        use imap_proto::Status;
        match status {
            Status::Ok => Ok(()),
            Status::Bad => Err(Error::Bad(format!("code: {code:?}, info: {information:?}"))),
            Status::No => Err(Error::No(format!("code: {code:?}, info: {information:?}"))),
            _ => Err(Error::Io(io::Error::other(format!(
                "status: {status:?}, code: {code:?}, information: {information:?}"
            )))),
        }
    }
}

fn validate_str(value: &str) -> Result<String> {
    let quoted = quote!(value);
    if quoted.find('\n').is_some() {
        return Err(Error::Validate(ValidateError('\n')));
    }
    if quoted.find('\r').is_some() {
        return Err(Error::Validate(ValidateError('\r')));
    }
    Ok(quoted)
}
