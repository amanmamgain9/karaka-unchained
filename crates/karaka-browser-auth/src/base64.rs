//! Base64, both alphabets, as the JWT and PEM pieces need them.

const STANDARD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn decode_with(text: &str, alphabet: &[u8; 64]) -> Option<Vec<u8>> {
    let text = text.trim_end_matches('=');
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut buffer = 0_u32;
    let mut bits = 0_u32;
    for byte in text.bytes() {
        let value = alphabet.iter().position(|&known| known == byte)? as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    // A lone sixth of a byte can't be the end of any encoding.
    if bits >= 6 {
        return None;
    }
    Some(out)
}

/// Unpadded base64url, as a JWS segment is written.
pub(crate) fn decode_url(text: &str) -> Option<Vec<u8>> {
    if text.contains('=') {
        return None;
    }
    decode_with(text, URL)
}

/// Standard base64, whitespace ignored, as PEM bodies are written.
pub(crate) fn decode_standard(text: &str) -> Option<Vec<u8>> {
    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    decode_with(&compact, STANDARD)
}

/// Unpadded base64url of `bytes`.
pub fn encode_url(bytes: &[u8]) -> String {
    encode_with(bytes, URL, false)
}

/// Padded standard base64 of `bytes`.
pub fn encode_standard(bytes: &[u8]) -> String {
    encode_with(bytes, STANDARD, true)
}

fn encode_with(bytes: &[u8], alphabet: &[u8; 64], pad: bool) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = chunk
            .iter()
            .enumerate()
            .fold(0_u32, |value, (index, byte)| {
                value | (u32::from(*byte) << (16 - 8 * index))
            });
        let chars = chunk.len() + 1;
        for index in 0..4 {
            if index < chars {
                out.push(alphabet[((value >> (18 - 6 * index)) & 63) as usize] as char);
            } else if pad {
                out.push('=');
            }
        }
    }
    out
}
