use serde::Serialize;

#[derive(Debug)]
pub(crate) struct ConnectionBudgetExceeded;
impl std::fmt::Display for ConnectionBudgetExceeded {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("IMAP connection byte budget exhausted")
    }
}
impl std::error::Error for ConnectionBudgetExceeded {}

#[derive(Debug, Clone, Serialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct MailError {
    pub code: &'static str,
    pub message: &'static str,
    pub retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Box<ErrorDetails>>,
}

pub const MAX_INPUT_VIOLATIONS: usize = 8;

/// Fixed reasons and numeric measurements keep argument values out of errors.
#[derive(Debug, Clone, Serialize)]
pub struct InputViolation {
    pub field: String,
    pub reason: &'static str,
    pub action: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_type: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimum: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maximum: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub related_field: Option<&'static str>,
}
impl InputViolation {
    pub fn new(field: &str, reason: &'static str, action: &'static str) -> Self {
        Self {
            field: field.into(),
            reason,
            action,
            expected: None,
            actual_type: None,
            limit: None,
            actual: None,
            unit: None,
            minimum: None,
            maximum: None,
            related_field: None,
        }
    }
    pub fn with_length(mut self, limit: usize, actual: usize, unit: &'static str) -> Self {
        self.limit = Some(limit);
        self.actual = Some(actual);
        self.unit = Some(unit);
        self
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitKind {
    RawMessageBytes,
    DecodedAttachmentBytes,
    ConnectionBytes,
    PartFetchBytes,
    DecodedBodyBytes,
    MimeStructure,
    HeaderBytes,
    WireResponseBytes,
    ScanItems,
    CacheQuota,
}

#[derive(Debug, Clone, Serialize)]
pub struct ErrorDetails {
    #[serde(flatten)]
    pub primary: Option<InputViolation>,
    /// Together with primary, at most MAX_INPUT_VIOLATIONS entries.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub additional_violations: Vec<InputViolation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_kind: Option<LimitKind>,
}
pub type Result<T> = std::result::Result<T, MailError>;
impl MailError {
    pub fn attachment_storage_unavailable() -> Self {
        Self {
            code: "ATTACHMENT_STORAGE_UNAVAILABLE",
            message: "Attachment download storage is unavailable. Contact the service operator.",
            retryable: false,
            details: None,
        }
    }
    pub fn account_unavailable() -> Self {
        Self {
            code: "ACCOUNT_UNAVAILABLE",
            message: "The account is unavailable. List accessible accounts and choose one.",
            retryable: false,
            details: None,
        }
    }
    pub fn input() -> Self {
        Self {
            code: "INVALID_INPUT",
            message: "Check the tool arguments and configured limits.",
            retryable: false,
            details: None,
        }
    }
    pub fn input_field(violation: InputViolation) -> Self {
        Self::input_fields(vec![violation])
    }
    pub fn input_fields(mut violations: Vec<InputViolation>) -> Self {
        violations.truncate(MAX_INPUT_VIOLATIONS);
        if violations.is_empty() {
            return Self::input();
        }
        let primary = violations.remove(0);
        Self {
            details: Some(Box::new(ErrorDetails {
                primary: Some(primary),
                additional_violations: violations,
                limit_kind: None,
            })),
            ..Self::input()
        }
    }
    pub fn backend() -> Self {
        Self {
            code: "MAIL_BACKEND",
            message: "The mail server did not complete the operation. Check service diagnostics.",
            retryable: false,
            details: None,
        }
    }
    pub fn storage() -> Self {
        Self {
            code: "STORAGE_UNAVAILABLE",
            message: "Durable operation storage is unavailable.",
            retryable: false,
            details: None,
        }
    }
    pub fn limit() -> Self {
        Self {
            code: "LIMIT_EXCEEDED",
            message: "The response exceeds the configured byte or item limit.",
            retryable: false,
            details: None,
        }
    }
    pub fn limit_kind(kind: LimitKind) -> Self {
        Self {
            details: Some(Box::new(ErrorDetails {
                primary: None,
                additional_violations: Vec::new(),
                limit_kind: Some(kind),
            })),
            ..Self::limit()
        }
    }
    pub fn limit_with(kind: LimitKind, limit: usize, actual: usize) -> Self {
        let mut error = Self::limit_kind(kind);
        let unit = if matches!(kind, LimitKind::ScanItems) {
            "items"
        } else {
            "bytes"
        };
        error.details.as_mut().unwrap().primary = Some(
            InputViolation::new("", "resource_limit", "reduce_request")
                .with_length(limit, actual, unit),
        );
        error
    }
    pub fn timeout() -> Self {
        Self {
            code: "TIMEOUT",
            message: "The operation timed out. Inspect operation status before retrying a write.",
            retryable: false,
            details: None,
        }
    }
}
