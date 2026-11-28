//! A WebSocket, RFC 6455, as `ws`'s server behaves without extensions:
//! the handshake's accept key, masked client frames, fragmented messages,
//! ping answered with pong, the close handshake, a payload limit (1009),
//! invalid UTF-8 (1007) and protocol errors (1002). (Karaka:
//! `WebSocketServer` from `ws`, with `perMessageDeflate` off, its default.)
//!
//! It is written here rather than taken from a crate: the server side of
//! the protocol is small, and every case `ws` handles is visible below.

use ring::digest::{SHA1_FOR_LEGACY_USE_ONLY, digest};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The handshake's GUID. (RFC 6455 §1.3.)
const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// `Sec-WebSocket-Accept` for `key`.
pub fn accept_key(key: &str) -> String {
    let hash = digest(&SHA1_FOR_LEGACY_USE_ONLY, format!("{key}{GUID}").as_bytes());
    base64(hash.as_ref())
}

/// Whether `key` is a valid `Sec-WebSocket-Key`: 16 bytes, base64. (ws:
/// `/^[+/0-9A-Za-z]{22}==$/`.)
pub fn valid_key(key: &str) -> bool {
    let bytes = key.as_bytes();
    bytes.len() == 24
        && bytes[22..] == *b"=="
        && bytes[..22]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'+' || *byte == b'/')
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let value = chunk
            .iter()
            .enumerate()
            .fold(0_u32, |value, (index, byte)| {
                value | (u32::from(*byte) << (16 - 8 * index))
            });
        for index in 0..4 {
            if index <= chunk.len() {
                out.push(ALPHABET[((value >> (18 - 6 * index)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

pub const OP_CONTINUATION: u8 = 0x0;
pub const OP_TEXT: u8 = 0x1;
pub const OP_BINARY: u8 = 0x2;
pub const OP_CLOSE: u8 = 0x8;
pub const OP_PING: u8 = 0x9;
pub const OP_PONG: u8 = 0xa;

/// What a peer sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Incoming {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong,
    /// A close frame: its code, if it had one, and its reason.
    Close(Option<u16>, String),
}

/// Why reading stopped: the close code to send, or the connection gone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadError {
    /// A protocol failure, closed with this code. (ws's receiver errors.)
    Close(u16),
    /// The connection ended or failed.
    Gone,
}

/// Reads one frame's header and payload, unmasking it.
async fn frame<R: AsyncRead + Unpin>(
    io: &mut R,
    masked: bool,
    room: usize,
) -> Result<(bool, u8, Vec<u8>), ReadError> {
    let gone = |_| ReadError::Gone;
    let mut head = [0_u8; 2];
    io.read_exact(&mut head).await.map_err(gone)?;
    let fin = head[0] & 0x80 != 0;
    if head[0] & 0x70 != 0 {
        return Err(ReadError::Close(1002));
    }
    let opcode = head[0] & 0x0f;
    if (head[1] & 0x80 != 0) != masked {
        return Err(ReadError::Close(1002));
    }
    let control = opcode >= 0x8;
    if !matches!(
        opcode,
        OP_CONTINUATION | OP_TEXT | OP_BINARY | OP_CLOSE | OP_PING | OP_PONG
    ) {
        return Err(ReadError::Close(1002));
    }
    let mut length = u64::from(head[1] & 0x7f);
    if control && (!fin || length > 125) {
        return Err(ReadError::Close(1002));
    }
    if length == 126 {
        let mut bytes = [0_u8; 2];
        io.read_exact(&mut bytes).await.map_err(gone)?;
        length = u64::from(u16::from_be_bytes(bytes));
    } else if length == 127 {
        let mut bytes = [0_u8; 8];
        io.read_exact(&mut bytes).await.map_err(gone)?;
        length = u64::from_be_bytes(bytes);
        if length >> 63 != 0 {
            return Err(ReadError::Close(1009));
        }
    }
    // ws counts a message's data frames against its limit.
    if !control && length > room as u64 {
        return Err(ReadError::Close(1009));
    }
    let mut mask = [0_u8; 4];
    if masked {
        io.read_exact(&mut mask).await.map_err(gone)?;
    }
    let mut payload = vec![0_u8; length as usize];
    io.read_exact(&mut payload).await.map_err(gone)?;
    if masked {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
    }
    Ok((fin, opcode, payload))
}

/// Whether `code` may be sent in a close frame. (ws: `isValidStatusCode`.)
pub fn valid_close_code(code: u16) -> bool {
    matches!(code, 1000..=1003 | 1007..=1014 | 3000..=4999)
}

/// Reads frames from a peer that masks them (a client) or doesn't (a
/// server), at most `max` bytes per message.
pub struct Reader<R> {
    io: R,
    masked: bool,
    max: usize,
}

impl<R: AsyncRead + Unpin> Reader<R> {
    pub fn new(io: R, masked: bool, max: usize) -> Self {
        Self { io, masked, max }
    }

    /// The next message or control frame.
    ///
    /// # Errors
    ///
    /// The peer broke the protocol, or went.
    pub async fn read(&mut self) -> Result<Incoming, ReadError> {
        let mut message: Option<(u8, Vec<u8>)> = None;
        loop {
            let used = message.as_ref().map_or(0, |(_, data)| data.len());
            let (fin, opcode, payload) = frame(&mut self.io, self.masked, self.max - used).await?;
            match opcode {
                OP_PING => return Ok(Incoming::Ping(payload)),
                OP_PONG => return Ok(Incoming::Pong),
                OP_CLOSE => return close_of(&payload),
                OP_CONTINUATION => {
                    let Some((_, data)) = &mut message else {
                        return Err(ReadError::Close(1002));
                    };
                    data.extend_from_slice(&payload);
                }
                _ => {
                    if message.is_some() {
                        return Err(ReadError::Close(1002));
                    }
                    message = Some((opcode, payload));
                }
            }
            if fin && let Some((opcode, data)) = message.take() {
                return if opcode == OP_TEXT {
                    String::from_utf8(data)
                        .map(Incoming::Text)
                        .map_err(|_| ReadError::Close(1007))
                } else {
                    Ok(Incoming::Binary(data))
                };
            }
        }
    }
}

fn close_of(payload: &[u8]) -> Result<Incoming, ReadError> {
    match payload.len() {
        0 => Ok(Incoming::Close(None, String::new())),
        1 => Err(ReadError::Close(1002)),
        _ => {
            let code = u16::from_be_bytes([payload[0], payload[1]]);
            if !valid_close_code(code) {
                return Err(ReadError::Close(1002));
            }
            let reason =
                String::from_utf8(payload[2..].to_vec()).map_err(|_| ReadError::Close(1007))?;
            Ok(Incoming::Close(Some(code), reason))
        }
    }
}

/// One frame, masked with `mask` when the writer is a client.
pub fn encode(opcode: u8, payload: &[u8], mask: Option<[u8; 4]>) -> Vec<u8> {
    let mut out = vec![0x80 | opcode];
    let masked = if mask.is_some() { 0x80 } else { 0 };
    match payload.len() {
        length @ 0..=125 => out.push(masked | length as u8),
        length @ 126..=0xffff => {
            out.push(masked | 126);
            out.extend_from_slice(&(length as u16).to_be_bytes());
        }
        length => {
            out.push(masked | 127);
            out.extend_from_slice(&(length as u64).to_be_bytes());
        }
    }
    match mask {
        Some(mask) => {
            out.extend_from_slice(&mask);
            out.extend(
                payload
                    .iter()
                    .enumerate()
                    .map(|(index, byte)| byte ^ mask[index % 4]),
            );
        }
        None => out.extend_from_slice(payload),
    }
    out
}

/// A close frame's payload: the code and reason, or nothing for no code.
pub fn close_payload(code: Option<u16>, reason: &str) -> Vec<u8> {
    match code {
        Some(code) => {
            let mut payload = code.to_be_bytes().to_vec();
            payload.extend_from_slice(reason.as_bytes());
            payload
        }
        None => Vec::new(),
    }
}

/// Writes one frame, as the server (unmasked).
///
/// # Errors
///
/// The connection failed.
pub async fn send<W: AsyncWrite + Unpin>(
    io: &mut W,
    opcode: u8,
    payload: &[u8],
) -> std::io::Result<()> {
    io.write_all(&encode(opcode, payload, None)).await?;
    io.flush().await
}
