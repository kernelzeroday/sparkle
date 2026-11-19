use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("could not reach the model server: {0}")]
    Http(#[from] reqwest::Error),
    #[error("the server returned {status}: {message}")]
    Api { status: u16, message: String },
    #[error("the server sent an error: {0}")]
    Stream(String),
    #[error("could not parse the server response: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Config(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl Error {
    /// Whether the same request may succeed if sent again shortly: the server could not be
    /// reached, timed out, asked us to slow down, or failed on its side.
    pub fn is_transient(&self) -> bool {
        match self {
            Error::Http(error) => error.is_connect() || error.is_timeout() || error.is_request(),
            Error::Api { status, .. } => matches!(status, 408 | 425 | 429 | 500..=599),
            Error::Stream(_) | Error::Json(_) | Error::Config(_) | Error::Io(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limits_and_server_failures_are_transient() {
        let api = |status| Error::Api { status, message: String::new() };
        for status in [408, 429, 500, 502, 503, 529] {
            assert!(api(status).is_transient(), "{status}");
        }
        for status in [400, 401, 403, 404, 422] {
            assert!(!api(status).is_transient(), "{status}");
        }
        assert!(!Error::Stream("bad".into()).is_transient());
        assert!(!Error::Config("bad".into()).is_transient());
    }
}
