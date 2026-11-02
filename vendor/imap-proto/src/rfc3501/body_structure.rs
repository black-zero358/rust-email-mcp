use crate::bounded::Budget;
use nom::{
    branch::alt,
    bytes::streaming::{tag, tag_no_case},
    character::streaming::char,
    combinator::{map, opt},
    sequence::{delimited, preceded},
    IResult, Parser,
};
use std::borrow::Cow;

use crate::{
    core::*,
    rfc3501::{to_owned_cow, AttributeValue, Envelope},
};

pub struct BodyFields<'a> {
    pub param: BodyParams<'a>,
    pub id: Option<Cow<'a, str>>,
    pub description: Option<Cow<'a, str>>,
    pub transfer_encoding: ContentEncoding<'a>,
    pub octets: u32,
}

impl<'a> BodyFields<'a> {
    #[cfg(test)]
    fn parse(i: &'a [u8]) -> IResult<&'a [u8], Self> {
        Self::parse_with_budget(i, &Budget::unlimited())
    }

    // body-fields     = body-fld-param SP body-fld-id SP body-fld-desc SP
    //                   body-fld-enc SP body-fld-octets
    fn parse_with_budget(i: &'a [u8], budget: &Budget<'_>) -> IResult<&'a [u8], Self> {
        let (i, (param, _, id, _, description, _, transfer_encoding, _, octets)) = (
            |i| body_param_bounded(i, budget),
            tag(" "),
            // body id seems to refer to the Message-ID or possibly Content-ID header, which
            // by the definition in RFC 2822 seems to resolve to all ASCII characters (through
            // a large amount of indirection which I did not have the patience to fully explore)
            |i| budget.nstring_utf8(i),
            tag(" "),
            // Per https://tools.ietf.org/html/rfc2045#section-8, description should be all ASCII
            |i| budget.nstring_utf8(i),
            tag(" "),
            |i| ContentEncoding::parse_with_budget(i, budget),
            tag(" "),
            number,
        )
            .parse(i)?;
        Ok((
            i,
            BodyFields {
                param,
                id,
                description,
                transfer_encoding,
                octets,
            },
        ))
    }

    pub fn into_owned(self) -> BodyFields<'static> {
        BodyFields {
            param: body_param_owned(self.param),
            id: self.id.map(to_owned_cow),
            description: self.description.map(to_owned_cow),
            transfer_encoding: self.transfer_encoding.into_owned(),
            octets: self.octets,
        }
    }
}

pub struct BodyExt1Part<'a> {
    pub md5: Option<Cow<'a, str>>,
    pub disposition: Option<ContentDisposition<'a>>,
    pub language: Option<Vec<Cow<'a, str>>>,
    pub location: Option<Cow<'a, str>>,
    pub extension: Option<BodyExtension<'a>>,
}

impl<'a> BodyExt1Part<'a> {
    #[cfg(test)]
    fn parse(i: &'a [u8]) -> IResult<&'a [u8], Self> {
        Self::parse_with_budget(i, &Budget::unlimited())
    }

    // body-ext-1part  = body-fld-md5 [SP body-fld-dsp [SP body-fld-lang
    //                   [SP body-fld-loc *(SP body-extension)]]]
    //                     ; MUST NOT be returned on non-extensible
    //                     ; "BODY" fetch
    fn parse_with_budget(i: &'a [u8], budget: &Budget<'_>) -> IResult<&'a [u8], Self> {
        let (i, (md5, disposition, language, location, extension)) = (
            // Per RFC 1864, MD5 values are base64-encoded
            opt_opt(preceded(tag(" "), |i| budget.nstring_utf8(i))),
            opt_opt(preceded(tag(" "), |i| body_disposition_bounded(i, budget))),
            opt_opt(preceded(tag(" "), |i| body_lang_bounded(i, budget))),
            // Location appears to reference a URL, which by RFC 1738 (section 2.2) should be ASCII
            opt_opt(preceded(tag(" "), |i| budget.nstring_utf8(i))),
            opt(preceded(tag(" "), |i| {
                BodyExtension::parse_with_budget(i, budget)
            })),
        )
            .parse(i)?;
        Ok((
            i,
            BodyExt1Part {
                md5,
                disposition,
                language,
                location,
                extension,
            },
        ))
    }

    pub fn into_owned(self) -> BodyExt1Part<'static> {
        BodyExt1Part {
            md5: self.md5.map(to_owned_cow),
            disposition: self.disposition.map(|v| v.into_owned()),
            language: self
                .language
                .map(|v| v.into_iter().map(to_owned_cow).collect()),
            location: self.location.map(to_owned_cow),
            extension: self.extension.map(|v| v.into_owned()),
        }
    }
}

pub struct BodyExtMPart<'a> {
    pub param: BodyParams<'a>,
    pub disposition: Option<ContentDisposition<'a>>,
    pub language: Option<Vec<Cow<'a, str>>>,
    pub location: Option<Cow<'a, str>>,
    pub extension: Option<BodyExtension<'a>>,
}

impl<'a> BodyExtMPart<'a> {
    #[cfg(test)]
    fn parse(i: &'a [u8]) -> IResult<&'a [u8], Self> {
        Self::parse_with_budget(i, &Budget::unlimited())
    }

    // body-ext-mpart  = body-fld-param [SP body-fld-dsp [SP body-fld-lang
    //                   [SP body-fld-loc *(SP body-extension)]]]
    //                     ; MUST NOT be returned on non-extensible
    //                     ; "BODY" fetch
    fn parse_with_budget(i: &'a [u8], budget: &Budget<'_>) -> IResult<&'a [u8], Self> {
        let (i, (param, disposition, language, location, extension)) = (
            opt_opt(preceded(tag(" "), |i| body_param_bounded(i, budget))),
            opt_opt(preceded(tag(" "), |i| body_disposition_bounded(i, budget))),
            opt_opt(preceded(tag(" "), |i| body_lang_bounded(i, budget))),
            // Location appears to reference a URL, which by RFC 1738 (section 2.2) should be ASCII
            opt_opt(preceded(tag(" "), |i| budget.nstring_utf8(i))),
            opt(preceded(tag(" "), |i| {
                BodyExtension::parse_with_budget(i, budget)
            })),
        )
            .parse(i)?;
        Ok((
            i,
            BodyExtMPart {
                param,
                disposition,
                language,
                location,
                extension,
            },
        ))
    }

    pub fn into_owned(self) -> BodyExtMPart<'static> {
        BodyExtMPart {
            param: body_param_owned(self.param),
            disposition: self.disposition.map(|v| v.into_owned()),
            language: self
                .language
                .map(|v| v.into_iter().map(to_owned_cow).collect()),
            location: self.location.map(to_owned_cow),
            extension: self.extension.map(|v| v.into_owned()),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum ContentEncoding<'a> {
    SevenBit,
    EightBit,
    Binary,
    Base64,
    QuotedPrintable,
    Other(Cow<'a, str>),
}

impl<'a> ContentEncoding<'a> {
    #[cfg(test)]
    fn parse(i: &'a [u8]) -> IResult<&'a [u8], Self> {
        Self::parse_with_budget(i, &Budget::unlimited())
    }

    // RFC 3501 defines `body-fld-enc` as a string, never NIL, but some servers send
    // NIL for a part with no Content-Transfer-Encoding header (seen on Apple iCloud;
    // imap-codec records the same from maddy). RFC 2045 s6.1 defaults that to 7bit.
    fn parse_with_budget(i: &'a [u8], budget: &Budget<'_>) -> IResult<&'a [u8], Self> {
        alt((
            map(nil, |_| ContentEncoding::SevenBit),
            delimited(
                char('"'),
                alt((
                    map(tag_no_case("7BIT"), |_| ContentEncoding::SevenBit),
                    map(tag_no_case("8BIT"), |_| ContentEncoding::EightBit),
                    map(tag_no_case("BINARY"), |_| ContentEncoding::Binary),
                    map(tag_no_case("BASE64"), |_| ContentEncoding::Base64),
                    map(tag_no_case("QUOTED-PRINTABLE"), |_| {
                        ContentEncoding::QuotedPrintable
                    }),
                )),
                char('"'),
            ),
            map(|i| budget.string_utf8(i), ContentEncoding::Other),
        ))
        .parse(i)
    }

    pub fn into_owned(self) -> ContentEncoding<'static> {
        match self {
            ContentEncoding::SevenBit => ContentEncoding::SevenBit,
            ContentEncoding::EightBit => ContentEncoding::EightBit,
            ContentEncoding::Binary => ContentEncoding::Binary,
            ContentEncoding::Base64 => ContentEncoding::Base64,
            ContentEncoding::QuotedPrintable => ContentEncoding::QuotedPrintable,
            ContentEncoding::Other(v) => ContentEncoding::Other(to_owned_cow(v)),
        }
    }
}

fn body_lang_bounded<'a>(
    i: &'a [u8],
    budget: &Budget<'_>,
) -> IResult<&'a [u8], Option<Vec<Cow<'a, str>>>> {
    alt((
        // body language seems to refer to RFC 3066 language tags, which should be ASCII-only
        |i| {
            let (rest, value) = budget.nstring_utf8(i)?;
            let value = if let Some(value) = value {
                Some(budget.single(i, value)?)
            } else {
                None
            };
            Ok((rest, value))
        },
        map(|i| budget.list(i, |i| budget.string_utf8(i)), Option::from),
    ))
    .parse(i)
}

fn body_param_bounded<'a>(i: &'a [u8], budget: &Budget<'_>) -> IResult<&'a [u8], BodyParams<'a>> {
    alt((
        map(nil, |_| None),
        map(
            |i| {
                budget.list(
                    i,
                    map(
                        (
                            |i| {
                                budget.parameter(i)?;
                                budget.string_utf8(i)
                            },
                            tag(" "),
                            |i| budget.string_utf8(i),
                        ),
                        |(key, _, val)| (key, val),
                    ),
                )
            },
            Option::from,
        ),
    ))
    .parse(i)
}

#[derive(Debug, Eq, PartialEq)]
pub enum BodyExtension<'a> {
    Num(u32),
    Str(Option<Cow<'a, str>>),
    List(Vec<BodyExtension<'a>>),
}

impl<'a> BodyExtension<'a> {
    #[cfg(test)]
    fn parse(i: &'a [u8]) -> IResult<&'a [u8], Self> {
        Self::parse_with_budget(i, &Budget::unlimited())
    }

    fn parse_with_budget(i: &'a [u8], budget: &Budget<'_>) -> IResult<&'a [u8], Self> {
        let _guard = budget.enter(i)?;
        alt((
            map(number, BodyExtension::Num),
            // Cannot find documentation on character encoding for body extension values.
            // So far, assuming UTF-8 seems fine, please report if you run into issues here.
            map(|i| budget.nstring_utf8(i), BodyExtension::Str),
            map(
                |i| budget.list(i, |i| BodyExtension::parse_with_budget(i, budget)),
                BodyExtension::List,
            ),
        ))
        .parse(i)
    }

    pub fn into_owned(self) -> BodyExtension<'static> {
        match self {
            BodyExtension::Num(v) => BodyExtension::Num(v),
            BodyExtension::Str(v) => BodyExtension::Str(v.map(to_owned_cow)),
            BodyExtension::List(v) => {
                BodyExtension::List(v.into_iter().map(|v| v.into_owned()).collect())
            }
        }
    }
}

fn body_disposition_bounded<'a>(
    i: &'a [u8],
    budget: &Budget<'_>,
) -> IResult<&'a [u8], Option<ContentDisposition<'a>>> {
    alt((
        map(nil, |_| None),
        paren_delimited(map(
            (
                |i| budget.string_utf8(i),
                tag(" "),
                |i| body_param_bounded(i, budget),
            ),
            |(ty, _, params)| Some(ContentDisposition { ty, params }),
        )),
    ))
    .parse(i)
}

fn body_type_basic<'a>(i: &'a [u8], budget: &Budget<'_>) -> IResult<&'a [u8], BodyStructure<'a>> {
    map(
        (
            |i| budget.string_utf8(i),
            tag(" "),
            |i| budget.string_utf8(i),
            tag(" "),
            |i| BodyFields::parse_with_budget(i, budget),
            |i| BodyExt1Part::parse_with_budget(i, budget),
        ),
        |(ty, _, subtype, _, fields, ext)| BodyStructure::Basic {
            common: BodyContentCommon {
                ty: ContentType {
                    ty,
                    subtype,
                    params: fields.param,
                },
                disposition: ext.disposition,
                language: ext.language,
                location: ext.location,
            },
            other: BodyContentSinglePart {
                id: fields.id,
                md5: ext.md5,
                octets: fields.octets,
                description: fields.description,
                transfer_encoding: fields.transfer_encoding,
            },
            extension: ext.extension,
        },
    )
    .parse(i)
}

fn body_type_text<'a>(i: &'a [u8], budget: &Budget<'_>) -> IResult<&'a [u8], BodyStructure<'a>> {
    map(
        (
            tag_no_case("\"TEXT\""),
            tag(" "),
            |i| budget.string_utf8(i),
            tag(" "),
            |i| BodyFields::parse_with_budget(i, budget),
            tag(" "),
            number,
            |i| BodyExt1Part::parse_with_budget(i, budget),
        ),
        |(_, _, subtype, _, fields, _, lines, ext)| BodyStructure::Text {
            common: BodyContentCommon {
                ty: ContentType {
                    ty: Cow::Borrowed("TEXT"),
                    subtype,
                    params: fields.param,
                },
                disposition: ext.disposition,
                language: ext.language,
                location: ext.location,
            },
            other: BodyContentSinglePart {
                id: fields.id,
                md5: ext.md5,
                octets: fields.octets,
                description: fields.description,
                transfer_encoding: fields.transfer_encoding,
            },
            lines,
            extension: ext.extension,
        },
    )
    .parse(i)
}

fn body_type_message<'a>(i: &'a [u8], budget: &Budget<'_>) -> IResult<&'a [u8], BodyStructure<'a>> {
    map(
        (
            tag_no_case("\"MESSAGE\" \"RFC822\""),
            tag(" "),
            |i| BodyFields::parse_with_budget(i, budget),
            tag(" "),
            |i| Envelope::parse_with_budget(i, budget),
            tag(" "),
            |i| {
                budget.boxed::<BodyStructure>(i)?;
                BodyStructure::parse_with_budget(i, budget)
            },
            tag(" "),
            number,
            |i| BodyExt1Part::parse_with_budget(i, budget),
        ),
        |(_, _, fields, _, envelope, _, body, _, lines, ext)| BodyStructure::Message {
            common: BodyContentCommon {
                ty: ContentType {
                    ty: Cow::Borrowed("MESSAGE"),
                    subtype: Cow::Borrowed("RFC822"),
                    params: fields.param,
                },
                disposition: ext.disposition,
                language: ext.language,
                location: ext.location,
            },
            other: BodyContentSinglePart {
                id: fields.id,
                md5: ext.md5,
                octets: fields.octets,
                description: fields.description,
                transfer_encoding: fields.transfer_encoding,
            },
            envelope,
            body: Box::new(body),
            lines,
            extension: ext.extension,
        },
    )
    .parse(i)
}

fn body_type_multipart<'a>(
    i: &'a [u8],
    budget: &Budget<'_>,
) -> IResult<&'a [u8], BodyStructure<'a>> {
    map(
        (
            |i| budget.adjacent_list(i, |i| BodyStructure::parse_with_budget(i, budget)),
            tag(" "),
            |i| budget.string_utf8(i),
            |i| BodyExtMPart::parse_with_budget(i, budget),
        ),
        |(bodies, _, subtype, ext)| BodyStructure::Multipart {
            common: BodyContentCommon {
                ty: ContentType {
                    ty: Cow::Borrowed("MULTIPART"),
                    subtype,
                    params: ext.param,
                },
                disposition: ext.disposition,
                language: ext.language,
                location: ext.location,
            },
            bodies,
            extension: ext.extension,
        },
    )
    .parse(i)
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Eq, PartialEq)]
pub enum BodyStructure<'a> {
    Basic {
        common: BodyContentCommon<'a>,
        other: BodyContentSinglePart<'a>,
        extension: Option<BodyExtension<'a>>,
    },
    Text {
        common: BodyContentCommon<'a>,
        other: BodyContentSinglePart<'a>,
        lines: u32,
        extension: Option<BodyExtension<'a>>,
    },
    Message {
        common: BodyContentCommon<'a>,
        other: BodyContentSinglePart<'a>,
        envelope: Envelope<'a>,
        body: Box<BodyStructure<'a>>,
        lines: u32,
        extension: Option<BodyExtension<'a>>,
    },
    Multipart {
        common: BodyContentCommon<'a>,
        bodies: Vec<BodyStructure<'a>>,
        extension: Option<BodyExtension<'a>>,
    },
}

impl<'a> BodyStructure<'a> {
    #[cfg(test)]
    pub(crate) fn parse(i: &'a [u8]) -> IResult<&'a [u8], Self> {
        Self::parse_with_budget(i, &Budget::unlimited())
    }

    pub(crate) fn parse_with_budget(i: &'a [u8], budget: &Budget<'_>) -> IResult<&'a [u8], Self> {
        let (i, _) = tag("(")(i)?;
        let _guard = budget.enter(i)?;
        let (i, value) = alt((
            |i| budget.attempt(i, |i| body_type_text(i, budget)),
            |i| budget.attempt(i, |i| body_type_message(i, budget)),
            |i| budget.attempt(i, |i| body_type_basic(i, budget)),
            |i| budget.attempt(i, |i| body_type_multipart(i, budget)),
        ))
        .parse(i)?;
        let (i, _) = tag(")")(i)?;
        Ok((i, value))
    }

    pub fn into_owned(self) -> BodyStructure<'static> {
        match self {
            BodyStructure::Basic {
                common,
                other,
                extension,
            } => BodyStructure::Basic {
                common: common.into_owned(),
                other: other.into_owned(),
                extension: extension.map(|v| v.into_owned()),
            },
            BodyStructure::Text {
                common,
                other,
                lines,
                extension,
            } => BodyStructure::Text {
                common: common.into_owned(),
                other: other.into_owned(),
                lines,
                extension: extension.map(|v| v.into_owned()),
            },
            BodyStructure::Message {
                common,
                other,
                envelope,
                body,
                lines,
                extension,
            } => BodyStructure::Message {
                common: common.into_owned(),
                other: other.into_owned(),
                envelope: envelope.into_owned(),
                body: Box::new(body.into_owned()),
                lines,
                extension: extension.map(|v| v.into_owned()),
            },
            BodyStructure::Multipart {
                common,
                bodies,
                extension,
            } => BodyStructure::Multipart {
                common: common.into_owned(),
                bodies: bodies.into_iter().map(|v| v.into_owned()).collect(),
                extension: extension.map(|v| v.into_owned()),
            },
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct BodyContentCommon<'a> {
    pub ty: ContentType<'a>,
    pub disposition: Option<ContentDisposition<'a>>,
    pub language: Option<Vec<Cow<'a, str>>>,
    pub location: Option<Cow<'a, str>>,
}

impl<'a> BodyContentCommon<'a> {
    pub fn into_owned(self) -> BodyContentCommon<'static> {
        BodyContentCommon {
            ty: self.ty.into_owned(),
            disposition: self.disposition.map(|v| v.into_owned()),
            language: self
                .language
                .map(|v| v.into_iter().map(to_owned_cow).collect()),
            location: self.location.map(to_owned_cow),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct BodyContentSinglePart<'a> {
    pub id: Option<Cow<'a, str>>,
    pub md5: Option<Cow<'a, str>>,
    pub description: Option<Cow<'a, str>>,
    pub transfer_encoding: ContentEncoding<'a>,
    pub octets: u32,
}

impl<'a> BodyContentSinglePart<'a> {
    pub fn into_owned(self) -> BodyContentSinglePart<'static> {
        BodyContentSinglePart {
            id: self.id.map(to_owned_cow),
            md5: self.md5.map(to_owned_cow),
            description: self.description.map(to_owned_cow),
            transfer_encoding: self.transfer_encoding.into_owned(),
            octets: self.octets,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct ContentType<'a> {
    pub ty: Cow<'a, str>,
    pub subtype: Cow<'a, str>,
    pub params: BodyParams<'a>,
}

impl<'a> ContentType<'a> {
    pub fn into_owned(self) -> ContentType<'static> {
        ContentType {
            ty: to_owned_cow(self.ty),
            subtype: to_owned_cow(self.subtype),
            params: body_param_owned(self.params),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct ContentDisposition<'a> {
    pub ty: Cow<'a, str>,
    pub params: BodyParams<'a>,
}

impl<'a> ContentDisposition<'a> {
    pub fn into_owned(self) -> ContentDisposition<'static> {
        ContentDisposition {
            ty: to_owned_cow(self.ty),
            params: body_param_owned(self.params),
        }
    }
}

pub type BodyParams<'a> = Option<Vec<(Cow<'a, str>, Cow<'a, str>)>>;

pub(crate) fn body_param_owned(v: BodyParams<'_>) -> BodyParams<'static> {
    v.map(|v| {
        v.into_iter()
            .map(|(k, v)| (to_owned_cow(k), to_owned_cow(v)))
            .collect()
    })
}

pub(crate) fn msg_att_body_structure<'a>(
    i: &'a [u8],
    budget: &Budget<'_>,
) -> IResult<&'a [u8], AttributeValue<'a>> {
    map(
        (tag_no_case("BODYSTRUCTURE "), |i| {
            BodyStructure::parse_with_budget(i, budget)
        }),
        |(_, body)| AttributeValue::BodyStructure(body),
    )
    .parse(i)
}

#[cfg(test)]
fn body_param(i: &[u8]) -> IResult<&[u8], BodyParams<'_>> {
    body_param_bounded(i, &Budget::unlimited())
}

#[cfg(test)]
fn body_disposition(i: &[u8]) -> IResult<&[u8], Option<ContentDisposition<'_>>> {
    body_disposition_bounded(i, &Budget::unlimited())
}

#[cfg(test)]
fn body_lang(i: &[u8]) -> IResult<&[u8], Option<Vec<Cow<'_, str>>>> {
    body_lang_bounded(i, &Budget::unlimited())
}
#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;

    const EMPTY: &[u8] = &[];

    // body-fld-param SP body-fld-id SP body-fld-desc SP body-fld-enc SP body-fld-octets
    const BODY_FIELDS: &str = r#"("foo" "bar") "id" "desc" "7BIT" 1337"#;
    const BODY_FIELD_PARAM_PAIR: (Cow<'_, str>, Cow<'_, str>) =
        (Cow::Borrowed("foo"), Cow::Borrowed("bar"));
    const BODY_FIELD_ID: Option<Cow<'_, str>> = Some(Cow::Borrowed("id"));
    const BODY_FIELD_DESC: Option<Cow<'_, str>> = Some(Cow::Borrowed("desc"));
    const BODY_FIELD_ENC: ContentEncoding = ContentEncoding::SevenBit;
    const BODY_FIELD_OCTETS: u32 = 1337;

    fn mock_body_text() -> (String, BodyStructure<'static>) {
        (
            format!(r#"("TEXT" "PLAIN" {BODY_FIELDS} 42)"#),
            BodyStructure::Text {
                common: BodyContentCommon {
                    ty: ContentType {
                        ty: Cow::Borrowed("TEXT"),
                        subtype: Cow::Borrowed("PLAIN"),
                        params: Some(vec![BODY_FIELD_PARAM_PAIR]),
                    },
                    disposition: None,
                    language: None,
                    location: None,
                },
                other: BodyContentSinglePart {
                    md5: None,
                    transfer_encoding: BODY_FIELD_ENC,
                    octets: BODY_FIELD_OCTETS,
                    id: BODY_FIELD_ID,
                    description: BODY_FIELD_DESC,
                },
                lines: 42,
                extension: None,
            },
        )
    }

    #[test]
    fn test_body_param_data() {
        assert_matches!(body_param(br#"NIL"#), Ok((EMPTY, None)));

        assert_matches!(
            body_param(br#"("foo" "bar")"#),
            Ok((EMPTY, Some(param))) => {
                assert_eq!(param, vec![(Cow::Borrowed("foo"), Cow::Borrowed("bar"))]);
            }
        );
    }

    #[test]
    fn test_body_lang_data() {
        assert_matches!(
            body_lang(br#""bob""#),
            Ok((EMPTY, Some(langs))) => {
                assert_eq!(langs, vec!["bob"]);
            }
        );

        assert_matches!(
            body_lang(br#"("one" "two")"#),
            Ok((EMPTY, Some(langs))) => {
                assert_eq!(langs, vec!["one", "two"]);
            }
        );

        assert_matches!(body_lang(br#"NIL"#), Ok((EMPTY, None)));
    }

    #[test]
    fn test_body_extension_data() {
        assert_matches!(
            BodyExtension::parse(br#""blah""#),
            Ok((EMPTY, BodyExtension::Str(Some(Cow::Borrowed("blah")))))
        );

        assert_matches!(
            BodyExtension::parse(br#"NIL"#),
            Ok((EMPTY, BodyExtension::Str(None)))
        );

        assert_matches!(
            BodyExtension::parse(br#"("hello")"#),
            Ok((EMPTY, BodyExtension::List(list))) => {
                assert_eq!(list, vec![BodyExtension::Str(Some(Cow::Borrowed("hello")))]);
            }
        );

        assert_matches!(
            BodyExtension::parse(br#"(1337)"#),
            Ok((EMPTY, BodyExtension::List(list))) => {
                assert_eq!(list, vec![BodyExtension::Num(1337)]);
            }
        );
    }

    #[test]
    fn test_body_disposition_data() {
        assert_matches!(body_disposition(br#"NIL"#), Ok((EMPTY, None)));

        assert_matches!(
            body_disposition(br#"("attachment" ("FILENAME" "pages.pdf"))"#),
            Ok((EMPTY, Some(disposition))) => {
                assert_eq!(disposition, ContentDisposition {
                    ty: Cow::Borrowed("attachment"),
                    params: Some(vec![
                        (Cow::Borrowed("FILENAME"), Cow::Borrowed("pages.pdf"))
                    ])
                });
            }
        );
    }

    // Apple iCloud sends NIL here, which the grammar does not allow.
    #[test]
    fn test_body_encoding_nil_is_seven_bit() {
        assert_matches!(
            ContentEncoding::parse(br"NIL"),
            Ok((EMPTY, ContentEncoding::SevenBit))
        );
    }

    // The whole response has to survive it, not just the field.
    #[test]
    fn test_body_structure_multipart_with_nil_encoding() {
        const BODY: &[u8] = br#"(("text" "plain" ("CHARSET" "UTF-8") NIL NIL NIL 1694 51 NIL NIL NIL NIL)("text" "html" ("CHARSET" "UTF-8") NIL NIL "quoted-printable" 5750 77 NIL NIL NIL NIL) "alternative" ("BOUNDARY" "94eb2c1235681e93cc0568edba59") NIL NIL NIL)"#;

        assert_matches!(
            BodyStructure::parse(BODY),
            Ok((EMPTY, BodyStructure::Multipart { bodies, .. })) => {
                assert_eq!(bodies.len(), 2);
                assert_matches!(
                    &bodies[0],
                    BodyStructure::Text { other, .. } => {
                        assert_eq!(other.transfer_encoding, ContentEncoding::SevenBit);
                    }
                );
            }
        );
    }

    #[test]
    fn test_body_structure_text() {
        let (body_str, body_struct) = mock_body_text();

        assert_matches!(
            BodyStructure::parse(body_str.as_bytes()),
            Ok((_, text)) => {
                assert_eq!(text, body_struct);
            }
        );
    }

    #[test]
    fn test_body_structure_text_with_ext() {
        let body_str = format!(r#"("TEXT" "PLAIN" {BODY_FIELDS} 42 NIL NIL NIL NIL)"#);
        let (_, text_body_struct) = mock_body_text();

        assert_matches!(
            BodyStructure::parse(body_str.as_bytes()),
            Ok((_, text)) => {
                assert_eq!(text, text_body_struct)
            }
        );
    }

    #[test]
    fn test_body_structure_basic() {
        const BODY: &[u8] = br#"("APPLICATION" "PDF" ("NAME" "pages.pdf") NIL NIL "BASE64" 38838 NIL ("attachment" ("FILENAME" "pages.pdf")) NIL NIL)"#;

        assert_matches!(
            BodyStructure::parse(BODY),
            Ok((_, basic)) => {
                assert_eq!(basic, BodyStructure::Basic {
                    common: BodyContentCommon {
                        ty: ContentType {
                            ty: Cow::Borrowed("APPLICATION"),
                            subtype: Cow::Borrowed("PDF"),
                            params: Some(vec![(Cow::Borrowed("NAME"), Cow::Borrowed("pages.pdf"))])
                        },
                        disposition: Some(ContentDisposition {
                            ty: Cow::Borrowed("attachment"),
                            params: Some(vec![(Cow::Borrowed("FILENAME"), Cow::Borrowed("pages.pdf"))])
                        }),
                        language: None,
                        location: None,
                    },
                    other: BodyContentSinglePart {
                        transfer_encoding: ContentEncoding::Base64,
                        octets: 38838,
                        id: None,
                        md5: None,
                        description: None,
                    },
                    extension: None,
                })
            }
        );
    }

    #[test]
    fn test_body_structure_message() {
        let (text_body_str, _) = mock_body_text();
        let envelope_str = r#"("Wed, 17 Jul 1996 02:23:25 -0700 (PDT)" "IMAP4rev1 WG mtg summary and minutes" (("Terry Gray" NIL "gray" "cac.washington.edu")) (("Terry Gray" NIL "gray" "cac.washington.edu")) (("Terry Gray" NIL "gray" "cac.washington.edu")) ((NIL NIL "imap" "cac.washington.edu")) ((NIL NIL "minutes" "CNRI.Reston.VA.US") ("John Klensin" NIL "KLENSIN" "MIT.EDU")) NIL NIL "<B27397-0100000@cac.washington.edu>")"#;
        let body_str =
            format!(r#"("MESSAGE" "RFC822" {BODY_FIELDS} {envelope_str} {text_body_str} 42)"#);

        assert_matches!(
            BodyStructure::parse(body_str.as_bytes()),
            Ok((_, BodyStructure::Message { .. }))
        );
    }

    #[test]
    fn test_body_structure_multipart() {
        let (text_body_str1, text_body_struct1) = mock_body_text();
        let (text_body_str2, text_body_struct2) = mock_body_text();
        let body_str =
            format!(r#"({text_body_str1}{text_body_str2} "ALTERNATIVE" NIL NIL NIL NIL)"#);

        assert_matches!(
            BodyStructure::parse(body_str.as_bytes()),
            Ok((_, multipart)) => {
                assert_eq!(multipart, BodyStructure::Multipart {
                    common: BodyContentCommon {
                        ty: ContentType {
                            ty: Cow::Borrowed("MULTIPART"),
                            subtype: Cow::Borrowed("ALTERNATIVE"),
                            params: None
                        },
                        language: None,
                        location: None,
                        disposition: None,
                    },
                    bodies: vec![
                        text_body_struct1,
                        text_body_struct2,
                    ],
                    extension: None
                });
            }
        );
    }
}
