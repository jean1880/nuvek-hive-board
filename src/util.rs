//! Small pure helpers: hashing, normalisation, char-safe truncation.

use sha2::{Digest, Sha256};

/// Lowercase, collapse whitespace, keep the first 300 characters (char-safe).
pub fn normalize_claim(s: &str) -> String {
    let joined = s
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    truncate_chars(&joined, 300)
}

/// Hex sha256 of the normalised claim: the dedupe key for findings.
pub fn content_hash(claim: &str) -> String {
    hex(&Sha256::digest(normalize_claim(claim).as_bytes()))
}

/// Hex sha256 of the exact text (audit rows: the text itself is not kept).
pub fn sha256_hex(s: &str) -> String {
    hex(&Sha256::digest(s.as_bytes()))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Truncate to at most `n` characters, never splitting a code point.
pub fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Slug rule carried over from v0.1: lowercase, alphanumerics plus `-`/`_`.
pub fn slugify(s: &str) -> String {
    s.trim()
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

pub fn clamp_limit(limit: Option<i64>, default: i64, max: i64) -> i64 {
    limit.unwrap_or(default).clamp(1, max)
}

/// Trim, drop empties and duplicates, cap count and length.
pub fn clean_list(items: Option<Vec<String>>, max_items: usize, max_len: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in items.unwrap_or_default() {
        let t = truncate_chars(raw.trim(), max_len);
        if !t.is_empty() && !out.contains(&t) {
            out.push(t);
        }
        if out.len() >= max_items {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_case_and_whitespace() {
        assert_eq!(normalize_claim("  Foo \n\t BAR  "), "foo bar");
        assert_eq!(content_hash("Foo   bar"), content_hash("foo BAR"));
    }

    #[test]
    fn truncation_is_char_safe() {
        let s = "é".repeat(400);
        assert_eq!(normalize_claim(&s).chars().count(), 300);
        assert_eq!(truncate_chars("日本語テキスト", 3), "日本語");
    }

    #[test]
    fn hash_is_hex_sha256() {
        let h = content_hash("x");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
