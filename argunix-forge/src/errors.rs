use std::fmt;

#[derive(Debug, thiserror::Error)]
pub enum ForgeError {
    #[error("missing required header `{0}`")]
    MissingHeader(&'static str),
    #[error("invalid header `{name}`: {reason}")]
    InvalidHeader { name: String, reason: String },
    #[error("HMAC signature mismatch")]
    BadSignature,
    #[error("malformed webhook payload: {0}")]
    BadPayload(#[source] serde_json::Error),
    #[error("invalid slug `{0}` in webhook payload: {1}")]
    InvalidSlug(String, argunix_domain::SlugError),
    #[error("invalid sha `{0}` in webhook payload: {1}")]
    InvalidSha(String, argunix_domain::ShaError),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("forge returned status {status} from {url}: {body}")]
    Api {
        status: u16,
        url: String,
        body: String,
    },
    #[error("forge token is unauthorised (401); pause repo until rotated")]
    Unauthorised,
}

impl ForgeError {
    pub(crate) fn invalid_header(name: impl Into<String>, reason: impl fmt::Display) -> Self {
        Self::InvalidHeader {
            name: name.into(),
            reason: reason.to_string(),
        }
    }
}
