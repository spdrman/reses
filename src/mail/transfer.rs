//! Transfer-encoding decoders, ported from CPython's binascii and email._encoded_words so that
//! malformed input decodes to the same bytes it does there.

const B64_PAD: u8 = b'=';

fn b64_value(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// `binascii.a2b_base64(data, strict_mode=strict)`; `None` where Python raises binascii.Error.
fn a2b_base64(data: &[u8], strict: bool) -> Option<Vec<u8>> {
    if strict && data.first() == Some(&B64_PAD) {
        return None;
    }
    let mut out = Vec::with_capacity(data.len() / 4 * 3 + 3);
    let mut quad_pos = 0u8;
    let mut leftchar = 0u8;
    let mut pads = 0u32;
    let mut padding_started = false;
    for (i, &ch) in data.iter().enumerate() {
        if ch == B64_PAD {
            padding_started = true;
            if strict && quad_pos == 0 {
                return None;
            }
            if quad_pos >= 2 {
                pads += 1;
                if u32::from(quad_pos) + pads >= 4 {
                    if strict && i + 1 < data.len() {
                        return None;
                    }
                    return Some(out);
                }
            }
            continue;
        }
        let Some(v) = b64_value(ch) else {
            if strict {
                return None;
            }
            continue;
        };
        if strict && padding_started {
            return None;
        }
        pads = 0;
        match quad_pos {
            0 => {
                quad_pos = 1;
                leftchar = v;
            }
            1 => {
                quad_pos = 2;
                out.push((leftchar << 2) | (v >> 4));
                leftchar = v & 0x0f;
            }
            2 => {
                quad_pos = 3;
                out.push((leftchar << 4) | (v >> 2));
                leftchar = v & 0x03;
            }
            _ => {
                quad_pos = 0;
                out.push((leftchar << 6) | v);
                leftchar = 0;
            }
        }
    }
    if quad_pos != 0 { None } else { Some(out) }
}

/// `email._encoded_words.decode_b`: strict with repaired padding, then lenient, then lenient
/// with extra padding, and finally the input unchanged.
pub(super) fn decode_b(encoded: &[u8]) -> Vec<u8> {
    let pad_err = encoded.len() % 4;
    let mut padded = encoded.to_vec();
    if pad_err != 0 {
        // b"==="[:4-pad_err]
        padded.extend(std::iter::repeat_n(B64_PAD, 4 - pad_err));
    }
    if let Some(v) = a2b_base64(&padded, true) {
        return v;
    }
    if let Some(v) = a2b_base64(encoded, false) {
        return v;
    }
    let mut more = encoded.to_vec();
    more.extend_from_slice(b"==");
    if let Some(v) = a2b_base64(&more, false) {
        return v;
    }
    encoded.to_vec()
}

/// A base64 body: Message.get_payload joins `bytes.splitlines()` and hands that to decode_b.
pub(super) fn decode_base64_body(payload: &[u8]) -> Vec<u8> {
    let joined: Vec<u8> = payload
        .iter()
        .copied()
        .filter(|&b| b != b'\r' && b != b'\n')
        .collect();
    decode_b(&joined)
}

fn hex(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}

/// `binascii.a2b_qp(data, header=False)`.
pub(super) fn a2b_qp(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    let n = data.len();
    while i < n {
        if data[i] == b'=' {
            i += 1;
            if i >= n {
                break;
            }
            if data[i] == b'\n' || data[i] == b'\r' {
                // Soft line break. A bare CR swallows everything up to the next LF.
                if data[i] != b'\n' {
                    while i < n && data[i] != b'\n' {
                        i += 1;
                    }
                }
                if i < n {
                    i += 1;
                }
            } else if data[i] == b'=' {
                out.push(b'=');
                i += 1;
            } else if i + 1 < n
                && let (Some(h), Some(l)) = (hex(data[i]), hex(data[i + 1]))
            {
                out.push((h << 4) | l);
                i += 2;
            } else {
                out.push(b'=');
            }
        } else {
            out.push(data[i]);
            i += 1;
        }
    }
    out
}

/// `email._encoded_words.decode_q`: underscores are spaces, `=XX` is a byte, anything else
/// stays as it is.
pub(super) fn decode_q(encoded: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(encoded.len());
    let mut i = 0;
    while i < encoded.len() {
        let b = encoded[i];
        if b == b'='
            && i + 2 < encoded.len()
            && let (Some(h), Some(l)) = (hex(encoded[i + 1]), hex(encoded[i + 2]))
        {
            out.push((h << 4) | l);
            i += 3;
            continue;
        }
        out.push(if b == b'_' { b' ' } else { b });
        i += 1;
    }
    out
}

/// `urllib.parse.unquote_to_bytes` on bytes.
pub(super) fn unquote_to_bytes(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'%'
            && i + 2 < s.len()
            && let (Some(h), Some(l)) = (hex(s[i + 1]), hex(s[i + 2]))
        {
            out.push((h << 4) | l);
            i += 3;
            continue;
        }
        out.push(s[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_like_python() {
        assert_eq!(decode_b(b"aGVsbG8="), b"hello");
        assert_eq!(decode_b(b"aGVsbG8"), b"hello");
        assert_eq!(decode_b(b"aGVs*bG8gd29ybGQ"), b"hello world");
        // Length one more than a multiple of four can't decode, so it comes back unchanged.
        assert_eq!(decode_b(b"aGVsb"), b"aGVsb");
        assert_eq!(decode_b(b"aGk=trailing"), b"hi");
    }

    #[test]
    fn quoted_printable_like_binascii() {
        assert_eq!(a2b_qp(b"a=3Db=\r\nc=\nd"), b"a=bcd");
        assert_eq!(a2b_qp(b"x=ZZ y==z end="), b"x=ZZ y=z end");
        assert_eq!(a2b_qp(b"lower=c3=a9"), "loweré".as_bytes());
        assert_eq!(a2b_qp(b"cr=\rgone\nkept"), b"crkept");
    }

    #[test]
    fn q_words() {
        assert_eq!(decode_q(b"a_b=3F=zz="), b"a b?=zz=");
    }
}
