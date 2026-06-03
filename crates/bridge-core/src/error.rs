use thiserror::Error;

#[derive(Error, Debug)]
pub enum BridgeError {
    #[error("Terminal error: {0}")]
    Terminal(String),

    #[error("Messaging client error: {0}")]
    Messaging(String),

    /// The platform rejected a message because its body exceeded the size
    /// limit. Distinguished from generic `Messaging` so the dispatcher can
    /// react by shrinking and retrying rather than just logging — a sizing
    /// miscalculation must never permanently wedge a message.
    #[error("Message too long: {0}")]
    MessageTooLong(String),

    #[error("Authentication error: {0}")]
    Auth(String),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Channel closed")]
    ChannelClosed,
}
