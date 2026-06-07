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
    ///
    /// `limit_hint` is the platform's stated maximum length when the error
    /// carries one (in the same unit the format's `measure` approximates), so
    /// the dispatcher can jump straight to a fitting size instead of stepping.
    /// `None` when the platform gives no number (e.g. Slack's `msg_too_long`),
    /// in which case the dispatcher escalates its trim adaptively.
    #[error("Message too long: {detail}")]
    MessageTooLong {
        detail: String,
        limit_hint: Option<usize>,
    },

    #[error("Authentication error: {0}")]
    Auth(String),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Channel closed")]
    ChannelClosed,
}
