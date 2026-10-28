use serde::Serialize;

#[derive(Debug, Clone, Serialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct MailError {
    pub code: &'static str,
    pub message: &'static str,
    pub retryable: bool,
}
pub type Result<T> = std::result::Result<T, MailError>;
impl MailError {
    pub fn input() -> Self {
        Self {
            code: "INVALID_INPUT",
            message: "Check the tool arguments and configured limits.",
            retryable: false,
        }
    }
    pub fn backend() -> Self {
        Self {
            code: "MAIL_BACKEND",
            message: "The mail server did not complete the operation. Check service diagnostics.",
            retryable: false,
        }
    }
    pub fn storage() -> Self {
        Self {
            code: "STORAGE_UNAVAILABLE",
            message: "Durable operation storage is unavailable.",
            retryable: false,
        }
    }
    pub fn limit() -> Self {
        Self {
            code: "LIMIT_EXCEEDED",
            message: "The response exceeds the configured byte or item limit.",
            retryable: false,
        }
    }
    pub fn timeout() -> Self {
        Self {
            code: "TIMEOUT",
            message: "The operation timed out. Inspect operation status before retrying a write.",
            retryable: false,
        }
    }
}
