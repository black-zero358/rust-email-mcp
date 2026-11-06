use std::fmt::{Formatter, Result as FmtResult};

use serde::{
    Deserialize, Serialize,
    de::{Deserializer, Error as DeError, MapAccess, SeqAccess, Visitor},
    ser::Serializer,
};

use crate::message::{Mailbox, Mailboxes};

impl Serialize for Mailbox {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Mailbox {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        enum Field {
            Name,
            Email,
        }

        const FIELDS: &[&str] = &["name", "email"];

        impl<'de> Deserialize<'de> for Field {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                struct FieldVisitor;

                impl Visitor<'_> for FieldVisitor {
                    type Value = Field;

                    fn expecting(&self, formatter: &mut Formatter<'_>) -> FmtResult {
                        formatter.write_str("'name' or 'email'")
                    }

                    fn visit_str<E>(self, value: &str) -> Result<Field, E>
                    where
                        E: DeError,
                    {
                        match value {
                            "name" => Ok(Field::Name),
                            "email" => Ok(Field::Email),
                            _ => Err(DeError::unknown_field(value, FIELDS)),
                        }
                    }
                }

                deserializer.deserialize_identifier(FieldVisitor)
            }
        }

        struct MailboxVisitor;

        impl<'de> Visitor<'de> for MailboxVisitor {
            type Value = Mailbox;

            fn expecting(&self, formatter: &mut Formatter<'_>) -> FmtResult {
                formatter.write_str("mailbox string or object")
            }

            fn visit_str<E>(self, s: &str) -> Result<Self::Value, E>
            where
                E: DeError,
            {
                s.parse().map_err(DeError::custom)
            }

            fn visit_map<V>(self, mut map: V) -> Result<Self::Value, V::Error>
            where
                V: MapAccess<'de>,
            {
                let mut name = None;
                let mut addr = None;
                while let Some(key) = map.next_key()? {
                    match key {
                        Field::Name => {
                            if name.is_some() {
                                return Err(DeError::duplicate_field("name"));
                            }
                            name = Some(map.next_value()?);
                        }
                        Field::Email => {
                            if addr.is_some() {
                                return Err(DeError::duplicate_field("email"));
                            }
                            addr = Some(map.next_value()?);
                        }
                    }
                }
                let addr = addr.ok_or_else(|| DeError::missing_field("email"))?;
                Ok(Mailbox::new(name, addr))
            }
        }

        deserializer.deserialize_any(MailboxVisitor)
    }
}

impl Serialize for Mailboxes {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Mailboxes {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct MailboxesVisitor;

        impl<'de> Visitor<'de> for MailboxesVisitor {
            type Value = Mailboxes;

            fn expecting(&self, formatter: &mut Formatter<'_>) -> FmtResult {
                formatter.write_str("mailboxes string or sequence")
            }

            fn visit_str<E>(self, s: &str) -> Result<Self::Value, E>
            where
                E: DeError,
            {
                s.parse().map_err(DeError::custom)
            }

            fn visit_seq<V>(self, mut seq: V) -> Result<Self::Value, V::Error>
            where
                V: SeqAccess<'de>,
            {
                let mut mboxes = Mailboxes::new();
                while let Some(mbox) = seq.next_element()? {
                    mboxes.push(mbox);
                }
                Ok(mboxes)
            }
        }

        deserializer.deserialize_any(MailboxesVisitor)
    }
}
