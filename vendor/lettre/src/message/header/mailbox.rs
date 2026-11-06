use email_encoding::headers::writer::EmailWriter;

use super::{Header, HeaderName, HeaderValue};
use crate::{
    BoxError,
    message::mailbox::{Mailbox, Mailboxes},
};

/// Header which can contains multiple mailboxes
pub trait MailboxesHeader {
    fn join_mailboxes(&mut self, other: Self);
}

macro_rules! mailbox_header {
    ($(#[$doc:meta])*($type_name: ident, $header_name: expr)) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct $type_name(Mailbox);

        impl Header for $type_name {
            fn name() -> HeaderName {
                HeaderName::new_from_ascii_str($header_name)
            }

            fn parse(s: &str) -> Result<Self, BoxError> {
                let mailbox: Mailbox = s.parse()?;
                Ok(Self(mailbox))
            }

            fn display(&self) -> HeaderValue {
                let mut encoded_value = String::new();
                let line_len = $header_name.len() + ": ".len();
                {
                    let mut w = EmailWriter::new(&mut encoded_value, line_len, 0, false);
                    self.0.encode(&mut w).expect("writing `Mailbox` returned an error");
                }

                HeaderValue::dangerous_new_pre_encoded(Self::name(), self.0.to_string(), encoded_value)
            }
        }

        impl std::convert::From<Mailbox> for $type_name {
            #[inline]
            fn from(mailbox: Mailbox) -> Self {
                Self(mailbox)
            }
        }

        impl std::convert::From<$type_name> for Mailbox {
            #[inline]
            fn from(this: $type_name) -> Mailbox {
                this.0
            }
        }
    };
}

macro_rules! mailboxes_header {
    ($(#[$doc:meta])*($type_name: ident, $header_name: expr)) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct $type_name(pub(crate) Mailboxes);

        impl MailboxesHeader for $type_name {
            fn join_mailboxes(&mut self, other: Self) {
                self.0.extend(other.0);
            }
        }

        impl Header for $type_name {
            fn name() -> HeaderName {
                HeaderName::new_from_ascii_str($header_name)
            }

            fn parse(s: &str) -> Result<Self, BoxError> {
                let mailbox: Mailboxes = s.parse()?;
                Ok(Self(mailbox))
            }

            fn display(&self) -> HeaderValue {
                let mut encoded_value = String::new();
                let line_len = $header_name.len() + ": ".len();
                {
                    let mut w = EmailWriter::new(&mut encoded_value, line_len, 0, false);
                    self.0.encode(&mut w).expect("writing `Mailboxes` returned an error");
                }

                HeaderValue::dangerous_new_pre_encoded(Self::name(), self.0.to_string(), encoded_value)
            }
        }

        impl std::convert::From<Mailboxes> for $type_name {
            #[inline]
            fn from(mailboxes: Mailboxes) -> Self {
                Self(mailboxes)
            }
        }

        impl std::convert::From<$type_name> for Mailboxes {
            #[inline]
            fn from(this: $type_name) -> Mailboxes {
                this.0
            }
        }
    };
}

mailbox_header! {
    /**

    `Sender` header

    This header contains [`Mailbox`] associated with sender.

    ```no_test
    header::Sender("Mr. Sender <sender@example.com>".parse().unwrap())
    ```
     */
    (Sender, "Sender")
}

mailboxes_header! {
    /**

    `From` header

    This header contains [`Mailboxes`].

     */
    (From, "From")
}

mailboxes_header! {
    /**

    `Reply-To` header

    This header contains [`Mailboxes`].

     */
    (ReplyTo, "Reply-To")
}

mailboxes_header! {
    /**

    `To` header

    This header contains [`Mailboxes`].

     */
    (To, "To")
}

mailboxes_header! {
    /**

    `Cc` header

    This header contains [`Mailboxes`].

     */
    (Cc, "Cc")
}

mailboxes_header! {
    /**

    `Bcc` header

    This header contains [`Mailboxes`].

     */
    (Bcc, "Bcc")
}
