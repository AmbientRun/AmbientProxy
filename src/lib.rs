pub mod bytes;
pub mod client;

#[cfg(feature = "server")]
pub mod configuration;

#[cfg(feature = "server")]
mod friendly_id;

pub mod paths;
pub mod protocol;

#[cfg(feature = "server")]
pub mod server;

pub mod streams;

#[cfg(feature = "server")]
pub mod telemetry;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Message too long: {0}")]
    MessageTooLong(u32),
    #[error("Bad bincode message format: {0:?}")]
    BadMsgFormat(#[from] bincode::Error),
    #[error("IO Error")]
    IOError(#[from] std::io::Error),
    #[error(transparent)]
    ConnectionError(#[from] quinn::ConnectionError),
    #[error(transparent)]
    ReadExactError(#[from] quinn::ReadExactError),
    #[error(transparent)]
    ReadToEndError(#[from] quinn::ReadToEndError),
    #[error(transparent)]
    WriteError(#[from] quinn::WriteError),
    #[error(transparent)]
    SendDatagramError(#[from] quinn::SendDatagramError),
    #[error(transparent)]
    TLSError(rustls::Error),
    // TODO: remove
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
