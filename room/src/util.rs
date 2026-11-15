//! Small helpers that would otherwise need extra dependencies.

/// A non-cryptographic generator for latency jitter and fault choices.
pub struct Rng(u64);

impl Rng {
    pub fn new() -> Rng {
        let mut seed = [0u8; 8];
        getrandom::fill(&mut seed).expect("operating system randomness");
        Rng(u64::from_le_bytes(seed) | 1)
    }

    pub fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [0, 1).
    pub fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

const BASE64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub fn base64url(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16 | (*chunk.get(1).unwrap_or(&0) as u32) << 8 | *chunk.get(2).unwrap_or(&0) as u32;
        for index in 0..=chunk.len() {
            out.push(BASE64URL[(n >> (18 - 6 * index) & 63) as usize] as char);
        }
    }
    out
}

/// An unguessable URL-safe secret, like Python's `secrets.token_urlsafe(24)`.
pub fn token() -> String {
    let mut bytes = [0u8; 24];
    getrandom::fill(&mut bytes).expect("operating system randomness");
    base64url(&bytes)
}

pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Unpadded base64url with a length that can decode.
pub fn is_base64url(text: &str) -> bool {
    !text.is_empty() && text.len() % 4 != 1 && text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// An approximation of Python's `str.isprintable()` without Unicode tables:
/// rejects control, separator, format and private-use characters, but not
/// unassigned code points.
pub fn printable(c: char) -> bool {
    if c == ' ' {
        return true;
    }
    if c.is_control() || c.is_whitespace() {
        return false;
    }
    !matches!(c as u32,
        0xAD | 0x600..=0x605 | 0x61C | 0x6DD | 0x70F | 0x890..=0x891 | 0x8E2 | 0x180E |
        0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 | 0x2066..=0x206F | 0xFEFF |
        0xFFF9..=0xFFFB | 0xFFFE..=0xFFFF | 0xE000..=0xF8FF | 0x110BD | 0x110CD | 0x13430..=0x1343F |
        0x1BCA0..=0x1BCA3 | 0x1D173..=0x1D17A | 0xE0000..=0xE007F | 0xF0000..=0x10FFFF)
}

/// Percent-decode a URL component. `plus` turns `+` into a space (query strings).
pub fn percent_decode(text: &str, plus: bool) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let hex = bytes.get(index + 1..index + 3)?;
                let hex = std::str::from_utf8(hex).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                index += 3;
            }
            b'+' if plus => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// The first non-empty value of `name` in a query string, like `parse_qs(...)[name][0]`.
pub fn query_value(query: &str, name: &str) -> Option<String> {
    query.split(['&', ';']).find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        if percent_decode(key, true)? != name || value.is_empty() {
            return None;
        }
        percent_decode(value, true)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_matches_rfc4648_without_padding() {
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
        assert_eq!(token().len(), 32);
        assert_ne!(token(), token());
    }

    #[test]
    fn payload_and_text_checks() {
        assert!(is_base64url("AAAA") && is_base64url("AA") && is_base64url("-_"));
        assert!(!is_base64url("") && !is_base64url("A") && !is_base64url("AA==") && !is_base64url("a+b/"));
        assert!(constant_time_eq(b"abc", b"abc") && !constant_time_eq(b"abc", b"abd") && !constant_time_eq(b"ab", b"abc"));
        assert!("Ada Ölçü 名前 ♠".chars().all(printable));
        for c in ['\u{7}', '\t', '\u{a0}', '\u{200b}', '\u{2028}', '\u{feff}', '\u{e000}'] {
            assert!(!printable(c), "{c:?}");
        }
    }

    #[test]
    fn query_and_percent_decoding() {
        assert_eq!(query_value("token=a%2Db&after=3", "token").as_deref(), Some("a-b"));
        assert_eq!(query_value("token=&token=x", "token").as_deref(), Some("x"));
        assert_eq!(query_value("after=3", "token"), None);
        assert_eq!(percent_decode("%2e%2E/x", false).as_deref(), Some("../x"));
        assert_eq!(percent_decode("%zz", false), None);
        assert_eq!(percent_decode("%ff", false), None);
    }

    #[test]
    fn rng_is_in_range() {
        let mut rng = Rng::new();
        for _ in 0..1000 {
            let x = rng.unit();
            assert!((0.0..1.0).contains(&x));
            assert!(rng.below(3) < 3);
        }
    }
}
