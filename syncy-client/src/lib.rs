use yrs::error::UpdateError;

pub mod repo;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("websocket error: {0}")]
    Ws(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("{0}")]
    Unsupported(String),
    #[error("invalid doc id: {0}")]
    InvalidDocId(#[from] uuid::Error),
    #[error("failed to decode message: {0}")]
    Decoding(#[from] yrs::encoding::read::Error),
    #[error("failed to apply update: {0}")]
    ApplyUpdate(#[from] UpdateError),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
