use thiserror::Error;

#[derive(Error, Debug)]
pub enum BridgeError {
    #[error("Terminal error: {0}")]
    Terminal(String),

    #[error("Messaging client error: {0}")]
    Messaging(String),

    #[error("Authentication error: {0}")]
    Auth(String),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Channel closed")]
    ChannelClosed,
}
