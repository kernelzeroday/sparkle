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
