//! Wire protocol between the bridge process (server) and an attach client
//! (a separate process running `cli-bridge --attach`).
//!
//! Frame layout (big-endian for simplicity; loopback-only so endianness is
//! moot in practice but big-endian is standard for network protocols):
//!
//! ```text
//!   byte 0:        message type tag
//!   bytes 1..5:    payload length (u32, max 16 MiB)
//!   bytes 5..5+n:  payload bytes
//! ```
//!
//! Why hand-rolled framing instead of e.g. tokio-tungstenite or length-prefix
//! crates: this is local-only, single-purpose, and we want no surprise
//! dependencies. The frame loop is < 100 lines and easy to fuzz.
//!
//! Why a 16 MiB cap: protects the server against a malicious or wedged peer
//! sending a 4 GiB length prefix; legitimate input/output frames are kilobytes.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Maximum payload size (16 MiB). PTY chunks are well under 4 KiB; this only
/// exists to bound a buggy or hostile peer.
pub const MAX_PAYLOAD: usize = 16 * 1024 * 1024;

/// Current protocol version. Bump on incompatible changes; both sides reject
/// mismatched values during the Hello handshake.
pub const PROTOCOL_VERSION: u8 = 1;

/// Length of the per-session shared token, in hex chars (32 chars = 16 bytes
/// of entropy, which is plenty for accidental-collision resistance on a
/// loopback socket).
pub const TOKEN_LEN: usize = 32;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tag {
    Hello = 1,
    HelloOk = 2,
    Output = 3,
    Input = 4,
    Resize = 5,
    Goodbye = 6,
}

impl Tag {
    fn from_u8(b: u8) -> Option<Self> {
        match b {
            1 => Some(Tag::Hello),
            2 => Some(Tag::HelloOk),
            3 => Some(Tag::Output),
            4 => Some(Tag::Input),
            5 => Some(Tag::Resize),
            6 => Some(Tag::Goodbye),
            _ => None,
        }
    }
}

/// Messages exchanged over the attach socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// Client → Server. Must be the first frame the client sends; carries the
    /// shared token and the client's initial terminal size.
    Hello {
        version: u8,
        cols: u16,
        rows: u16,
        token: String,
    },
    /// Server → Client. Sent in response to a valid Hello.
    HelloOk,
    /// Server → Client. Raw PTY output bytes.
    Output(Vec<u8>),
    /// Client → Server. Raw bytes the client's stdin produced.
    Input(Vec<u8>),
    /// Client → Server. Local terminal was resized.
    Resize { cols: u16, rows: u16 },
    /// Either direction. Indicates a clean disconnect.
    Goodbye,
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("unknown message tag: {0}")]
    UnknownTag(u8),
    #[error("payload too large: {0} bytes")]
    PayloadTooLarge(usize),
    #[error("malformed payload for {0:?}")]
    Malformed(&'static str),
}

/// Read one frame from the wire. Returns `Ok(None)` on a clean EOF before any
/// header bytes arrive — the caller should treat that as a normal disconnect.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Message>, FrameError> {
    let mut header = [0u8; 5];
    match r.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let tag = Tag::from_u8(header[0]).ok_or(FrameError::UnknownTag(header[0]))?;
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len > MAX_PAYLOAD {
        return Err(FrameError::PayloadTooLarge(len));
    }

    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;

    Ok(Some(decode(tag, buf)?))
}

/// Serialize and write one frame. Caller is responsible for `flush()` if they
/// need bytes on the wire immediately; we don't auto-flush so a sender can
/// coalesce many small messages.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    w: &mut W,
    msg: &Message,
) -> Result<(), FrameError> {
    let (tag, payload) = encode(msg);
    if payload.len() > MAX_PAYLOAD {
        return Err(FrameError::PayloadTooLarge(payload.len()));
    }
    let mut header = [0u8; 5];
    header[0] = tag as u8;
    header[1..5].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    w.write_all(&header).await?;
    w.write_all(&payload).await?;
    Ok(())
}

fn encode(msg: &Message) -> (Tag, Vec<u8>) {
    match msg {
        Message::Hello {
            version,
            cols,
            rows,
            token,
        } => {
            let token_bytes = token.as_bytes();
            let mut buf = Vec::with_capacity(1 + 2 + 2 + 1 + token_bytes.len());
            buf.push(*version);
            buf.extend_from_slice(&cols.to_be_bytes());
            buf.extend_from_slice(&rows.to_be_bytes());
            buf.push(token_bytes.len() as u8);
            buf.extend_from_slice(token_bytes);
            (Tag::Hello, buf)
        }
        Message::HelloOk => (Tag::HelloOk, Vec::new()),
        Message::Output(bytes) => (Tag::Output, bytes.clone()),
        Message::Input(bytes) => (Tag::Input, bytes.clone()),
        Message::Resize { cols, rows } => {
            let mut buf = Vec::with_capacity(4);
            buf.extend_from_slice(&cols.to_be_bytes());
            buf.extend_from_slice(&rows.to_be_bytes());
            (Tag::Resize, buf)
        }
        Message::Goodbye => (Tag::Goodbye, Vec::new()),
    }
}

fn decode(tag: Tag, buf: Vec<u8>) -> Result<Message, FrameError> {
    match tag {
        Tag::Hello => {
            // 1 (version) + 2 (cols) + 2 (rows) + 1 (token len) + N (token)
            if buf.len() < 6 {
                return Err(FrameError::Malformed("Hello"));
            }
            let version = buf[0];
            let cols = u16::from_be_bytes([buf[1], buf[2]]);
            let rows = u16::from_be_bytes([buf[3], buf[4]]);
            let tok_len = buf[5] as usize;
            if buf.len() != 6 + tok_len {
                return Err(FrameError::Malformed("Hello"));
            }
            let token = std::str::from_utf8(&buf[6..6 + tok_len])
                .map_err(|_| FrameError::Malformed("Hello"))?
                .to_string();
            Ok(Message::Hello {
                version,
                cols,
                rows,
                token,
            })
        }
        Tag::HelloOk => Ok(Message::HelloOk),
        Tag::Output => Ok(Message::Output(buf)),
        Tag::Input => Ok(Message::Input(buf)),
        Tag::Resize => {
            if buf.len() != 4 {
                return Err(FrameError::Malformed("Resize"));
            }
            Ok(Message::Resize {
                cols: u16::from_be_bytes([buf[0], buf[1]]),
                rows: u16::from_be_bytes([buf[2], buf[3]]),
            })
        }
        Tag::Goodbye => Ok(Message::Goodbye),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    async fn roundtrip(msg: Message) -> Message {
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, &msg).await.unwrap();
        let mut cursor = Cursor::new(buf);
        read_frame(&mut cursor).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn hello_roundtrip() {
        let m = Message::Hello {
            version: PROTOCOL_VERSION,
            cols: 120,
            rows: 24,
            token: "deadbeefcafefacefeedfeedfeedfeed".to_string(),
        };
        assert_eq!(roundtrip(m.clone()).await, m);
    }

    #[tokio::test]
    async fn output_input_roundtrip() {
        let m = Message::Output(b"\x1b[2Jhello world\n".to_vec());
        assert_eq!(roundtrip(m.clone()).await, m);
        let m = Message::Input(b"ls\r".to_vec());
        assert_eq!(roundtrip(m.clone()).await, m);
    }

    #[tokio::test]
    async fn resize_roundtrip() {
        let m = Message::Resize {
            cols: 200,
            rows: 50,
        };
        assert_eq!(roundtrip(m.clone()).await, m);
    }

    #[tokio::test]
    async fn empty_payload_roundtrips() {
        assert_eq!(roundtrip(Message::HelloOk).await, Message::HelloOk);
        assert_eq!(roundtrip(Message::Goodbye).await, Message::Goodbye);
    }

    #[tokio::test]
    async fn empty_output_roundtrips() {
        // 0-byte Output is unusual but valid (e.g. a flush sentinel).
        let m = Message::Output(Vec::new());
        assert_eq!(roundtrip(m.clone()).await, m);
    }

    #[tokio::test]
    async fn rejects_unknown_tag() {
        // Hand-craft a frame with tag=99 and zero payload.
        let bytes = vec![99u8, 0, 0, 0, 0];
        let mut cursor = Cursor::new(bytes);
        let err = read_frame(&mut cursor).await.unwrap_err();
        assert!(matches!(err, FrameError::UnknownTag(99)));
    }

    #[tokio::test]
    async fn rejects_oversize_payload() {
        // Header advertises 17 MiB; we should reject before allocating.
        let mut bytes = vec![Tag::Output as u8];
        bytes.extend_from_slice(&((MAX_PAYLOAD + 1) as u32).to_be_bytes());
        let mut cursor = Cursor::new(bytes);
        let err = read_frame(&mut cursor).await.unwrap_err();
        assert!(matches!(err, FrameError::PayloadTooLarge(_)));
    }

    #[tokio::test]
    async fn clean_eof_returns_none() {
        let mut cursor = Cursor::new(Vec::<u8>::new());
        assert!(read_frame(&mut cursor).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn rejects_malformed_resize() {
        // Resize with 3-byte payload (need 4).
        let mut bytes = vec![Tag::Resize as u8];
        bytes.extend_from_slice(&3u32.to_be_bytes());
        bytes.extend_from_slice(&[1, 2, 3]);
        let mut cursor = Cursor::new(bytes);
        let err = read_frame(&mut cursor).await.unwrap_err();
        assert!(matches!(err, FrameError::Malformed("Resize")));
    }
}
