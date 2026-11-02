//! Input budgets are selected by operation purpose, including reads used by writes.
use crate::error::{InputViolation, MailError, Result};

pub(crate) struct ReadInputLimits;
impl ReadInputLimits {
    pub(crate) const MAILBOX_CHARACTERS: usize = 512;
    pub(crate) const QUERY_CHARACTERS: usize = 512;
    pub(crate) const TEXT_UTF8_BYTES: usize = 2048;
    pub(crate) const MAILBOX_WIRE_BYTES: usize = 8192;
}

pub(crate) struct LegacyWriteInputLimits;
impl LegacyWriteInputLimits {
    pub(crate) const TEXT_BYTES: usize = 512;
}

#[derive(Clone, Copy)]
pub(crate) enum InputLimits {
    Read,
    LegacyWrite,
}

impl InputLimits {
    pub(crate) fn mailbox(self, value: &str, field: &str) -> Result<()> {
        self.text(value, field, ReadInputLimits::MAILBOX_CHARACTERS)
    }

    fn text(self, value: &str, field: &str, characters: usize) -> Result<()> {
        if matches!(self, Self::LegacyWrite) {
            return if value.len() > LegacyWriteInputLimits::TEXT_BYTES
                || value.contains(['\r', '\n', '\0'])
            {
                Err(MailError::input())
            } else {
                Ok(())
            };
        }
        let actual = value.chars().count();
        if actual > characters {
            return Err(MailError::input_field(
                InputViolation::new(field, "character_limit", "shorten_field").with_length(
                    characters,
                    actual,
                    "unicode_code_points",
                ),
            ));
        }
        if value.len() > ReadInputLimits::TEXT_UTF8_BYTES {
            return Err(MailError::input_field(
                InputViolation::new(field, "utf8_byte_limit", "shorten_field").with_length(
                    ReadInputLimits::TEXT_UTF8_BYTES,
                    value.len(),
                    "utf8_bytes",
                ),
            ));
        }
        if value.contains(['\r', '\n', '\0']) {
            return Err(MailError::input_field(InputViolation::new(
                field,
                "forbidden_control_character",
                "remove_control_characters",
            )));
        }
        Ok(())
    }
}

pub(crate) fn read_query(value: &str, field: &str) -> Result<()> {
    InputLimits::Read.text(value, field, ReadInputLimits::QUERY_CHARACTERS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn character_utf8_and_legacy_byte_limits_have_separate_units() {
        for unit in ["x", "中", "😀"] {
            assert!(read_query(&unit.repeat(512), "/query").is_ok());
            let error =
                serde_json::to_value(read_query(&unit.repeat(513), "/query").unwrap_err()).unwrap();
            assert_eq!(error["details"]["actual"], 513);
            assert_eq!(error["details"]["unit"], "unicode_code_points");
        }
        assert!(read_query(&"e\u{301}".repeat(256), "/query").is_ok());
        assert!(read_query(&"e\u{301}".repeat(257), "/query").is_err());
        // Test the independent byte guard with a larger character allowance.
        assert!(
            InputLimits::Read
                .text(&"😀".repeat(512), "/query", 1024)
                .is_ok()
        );
        let error = serde_json::to_value(
            InputLimits::Read
                .text(&"😀".repeat(513), "/query", 1024)
                .unwrap_err(),
        )
        .unwrap();
        assert_eq!(error["details"]["limit"], 2048);
        assert_eq!(error["details"]["actual"], 2052);
        assert_eq!(error["details"]["unit"], "utf8_bytes");
        for value in ["x".repeat(512), "中".repeat(170) + "xx", "😀".repeat(128)] {
            assert!(InputLimits::LegacyWrite.mailbox(&value, "/mailbox").is_ok());
            let error = InputLimits::LegacyWrite
                .mailbox(&(value + "x"), "/mailbox")
                .unwrap_err();
            assert!(error.details.is_none());
        }
    }
}
