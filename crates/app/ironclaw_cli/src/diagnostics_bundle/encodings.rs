//! Encoded spellings of a known secret, and lenient decoders for the last
//! check before the archive is written.
//!
//! Replacement can only remove spellings it can enumerate, so it covers the
//! encodings a log or JSON document realistically produces: JSON string
//! escaping (serde style, `ensure_ascii` style, every character `\uXXXX`),
//! percent-encoding (minimal upper/lower hex, form style, every byte), and
//! base64 (standard and URL-safe; any alignment, so a secret embedded in a
//! larger encoded blob such as `Basic base64(user:pass)` is found too).
//! Detection additionally decodes JSON escapes and percent-escapes leniently,
//! so a mixed or partial encoding that replacement missed still stops the
//! archive from being written.

const STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Every spelling of `secret` that replacement removes, `secret` included.
pub(super) fn forms(secret: &str) -> Vec<String> {
    let mut out = vec![secret.to_string()];
    // silent-ok: serializing a &str cannot fail; on the impossible error the
    // other spellings still apply.
    if let Ok(quoted) = serde_json::to_string(secret)
        && quoted.len() >= 2
    {
        out.push(quoted[1..quoted.len() - 1].to_string());
    }
    out.push(json_escape(secret, false, false));
    out.push(json_escape(secret, true, false));
    out.push(json_escape(secret, true, true));
    out.push(json_escape_ascii(secret));
    out.push(percent_encode(secret, false, true, false));
    out.push(percent_encode(secret, false, false, false));
    out.push(percent_encode(secret, false, true, true));
    out.push(percent_encode(secret, true, true, false));
    out.push(percent_encode(secret, true, false, false));
    for alphabet in [STD, URL] {
        let full = base64(secret.as_bytes(), alphabet);
        out.push(full.trim_end_matches('=').to_string());
        out.push(full);
        for shift in 0..3 {
            out.push(base64_core(secret.as_bytes(), shift, alphabet));
        }
    }
    out.retain(|form| !form.is_empty());
    out
}

/// `\uXXXX` for every character (`all`) or for none beyond JSON's required
/// escapes; `upper` picks the hex case.
fn json_escape(secret: &str, all: bool, upper: bool) -> String {
    let mut out = String::new();
    for ch in secret.chars() {
        if all {
            push_u_escapes(&mut out, ch, upper);
        } else {
            match ch {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '/' => out.push_str("\\/"),
                ch if ch.is_control() => push_u_escapes(&mut out, ch, upper),
                ch => out.push(ch),
            }
        }
    }
    out
}

/// Python `json.dumps` default: ASCII kept, everything else `\uXXXX`.
fn json_escape_ascii(secret: &str) -> String {
    let mut out = String::new();
    for ch in secret.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            ch if ch.is_ascii() && !ch.is_control() => out.push(ch),
            ch => push_u_escapes(&mut out, ch, false),
        }
    }
    out
}

fn push_u_escapes(out: &mut String, ch: char, upper: bool) {
    let mut units = [0u16; 2];
    for unit in ch.encode_utf16(&mut units) {
        if upper {
            out.push_str(&format!("\\u{unit:04X}"));
        } else {
            out.push_str(&format!("\\u{unit:04x}"));
        }
    }
}

fn percent_encode(secret: &str, every_byte: bool, upper: bool, plus_for_space: bool) -> String {
    let mut out = String::new();
    for byte in secret.bytes() {
        let unreserved = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~');
        if !every_byte && unreserved {
            out.push(char::from(byte));
        } else if plus_for_space && byte == b' ' {
            out.push('+');
        } else if upper {
            out.push_str(&format!("%{byte:02X}"));
        } else {
            out.push_str(&format!("%{byte:02x}"));
        }
    }
    out
}

fn base64(bytes: &[u8], alphabet: &[u8; 64]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for index in 0..4 {
            if index <= chunk.len() {
                out.push(char::from(
                    alphabet[((n >> (18 - 6 * index)) & 63) as usize],
                ));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The base64 characters that depend only on `bytes` when they start at
/// offset `shift` (mod 3) inside a larger encoded byte string: a substring of
/// every such encoding, whatever surrounds the secret.
fn base64_core(bytes: &[u8], shift: usize, alphabet: &[u8; 64]) -> String {
    let mut padded = vec![0u8; shift];
    padded.extend_from_slice(bytes);
    let encoded = base64(&padded, alphabet);
    let start = (8 * shift).div_ceil(6);
    let end = (8 * (shift + bytes.len())) / 6;
    encoded.get(start..end).unwrap_or_default().to_string()
}

/// Decoded views of `bytes` searched by the final check: the raw text, JSON
/// escapes decoded, percent-escapes decoded (with and without `+` as space),
/// and both decodings stacked.
pub(super) fn decoded_views(bytes: &[u8]) -> Vec<Vec<u8>> {
    let json = json_unescape(bytes);
    vec![
        percent_decode(bytes, false),
        percent_decode(bytes, true),
        percent_decode(&json, false),
        percent_decode(&json, true),
        json,
        bytes.to_vec(),
    ]
}

fn json_unescape(bytes: &[u8]) -> Vec<u8> {
    let mut units: Vec<u16> = Vec::new();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let flush = |units: &mut Vec<u16>, out: &mut Vec<u8>| {
        if !units.is_empty() {
            out.extend(String::from_utf16_lossy(units).into_bytes());
            units.clear();
        }
    };
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 1 < bytes.len() {
            let next = bytes[index + 1];
            if next == b'u'
                && let Some(hex) = bytes.get(index + 2..index + 6)
                && let Ok(text) = std::str::from_utf8(hex)
                && let Ok(unit) = u16::from_str_radix(text, 16)
            {
                units.push(unit);
                index += 6;
                continue;
            }
            flush(&mut units, &mut out);
            let decoded = match next {
                b'n' => Some(b'\n'),
                b't' => Some(b'\t'),
                b'r' => Some(b'\r'),
                b'"' | b'\\' | b'/' | b'\'' => Some(next),
                _ => None,
            };
            if let Some(decoded) = decoded {
                out.push(decoded);
                index += 2;
                continue;
            }
        }
        flush(&mut units, &mut out);
        out.push(bytes[index]);
        index += 1;
    }
    flush(&mut units, &mut out);
    out
}

fn percent_decode(bytes: &[u8], plus_as_space: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some(hex) = bytes.get(index + 1..index + 3)
            && let Ok(text) = std::str::from_utf8(hex)
            && let Ok(byte) = u8::from_str_radix(text, 16)
        {
            out.push(byte);
            index += 3;
            continue;
        }
        out.push(if plus_as_space && bytes[index] == b'+' {
            b' '
        } else {
            bytes[index]
        });
        index += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors_and_cores_survive_embedding() {
        assert_eq!(base64(b"foobar", STD), "Zm9vYmFy");
        assert_eq!(base64(b"fo", STD), "Zm8=");
        let secret = b"s3cr3t-value!";
        for prefix in ["", "u", "us", "user:"] {
            let mut blob = prefix.as_bytes().to_vec();
            blob.extend_from_slice(secret);
            blob.extend_from_slice(b"tail");
            let encoded = base64(&blob, STD);
            let core = base64_core(secret, prefix.len() % 3, STD);
            assert!(core.len() >= 8, "{core}");
            assert!(encoded.contains(&core), "{prefix}: {encoded} lacks {core}");
        }
    }

    #[test]
    fn decoders_undo_mixed_encodings() {
        let views = decoded_views(br#"abc%64e+f\"g"#);
        assert!(views.iter().any(|view| view == b"abcde f\"g"), "{views:?}");
    }
}
