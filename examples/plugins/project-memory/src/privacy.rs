//! Best-effort privacy guard for text bound for the project memory store.
//!
//! This is a heuristic filter, **not** a secret scanner: it catches obvious PEM
//! private key blocks, well-known vendor token prefixes, credential-bearing URLs
//! and `api_key = "..."`-style assignments with a plausible value. Ordinary
//! discussion of keys/passwords ("rotate your api_key") and harmless
//! placeholders (`example`, `<token>`, `REDACTED`, `***`) are allowed.
//! Errors name the rule that fired and never echo the secret.

use anyhow::Result;

/// Bound the work done on very large inputs.
const MAX_SCAN: usize = 1 << 20;
/// Values shorter than this are treated as placeholders rather than secrets.
const MIN_SECRET_LEN: usize = 8;

const ASSIGN_KEYS: &[&str] = &[
    "api_key",
    "apikey",
    "api-key",
    "access_token",
    "access-token",
    "auth_token",
    "client_secret",
    "password",
    "passwd",
    "secret",
];
const TOKEN_PREFIXES: &[&str] = &["sk-", "ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_"];
const PLACEHOLDERS: &[&str] = &[
    "example",
    "redacted",
    "placeholder",
    "password",
    "passwd",
    "secret",
    "token",
    "changeme",
    "dummy",
    "fake",
    "none",
    "null",
    "nil",
    "todo",
    "xxx",
    "***",
    "your_token",
    "your-api-key",
];
const MARKERS: &[&str] = &[
    "example",
    "redacted",
    "placeholder",
    "your_",
    "your-",
    "changeme",
    "dummy",
    "fake",
    "sample",
    "xxxx",
];

/// Returns `Err` when `text` appears to contain a live secret.
pub fn check_text(text: &str) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    let mut cut = text.len().min(MAX_SCAN);
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let scan = text[..cut].to_ascii_lowercase();
    let b = scan.as_bytes();
    let what = if has_private_key_block(&scan) {
        "a PEM private key block"
    } else if has_vendor_token(b) {
        "a vendor-prefixed API token"
    } else if has_credential_url(b) {
        "a URL with embedded credentials"
    } else if has_credential_assignment(b) {
        "a credential assignment with a value"
    } else {
        return Ok(());
    };
    Err(anyhow::anyhow!(
        "refusing to store text: detected {what}; remove or redact the secret and retry"
    ))
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= hay.len() || needle.is_empty() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

fn has_private_key_block(lower: &str) -> bool {
    lower.lines().any(|l| {
        l.contains("-----begin") && (l.contains("private key") || l.contains("pgp private"))
    })
}

fn has_vendor_token(b: &[u8]) -> bool {
    for prefix in TOKEN_PREFIXES {
        let mut from = 0;
        while let Some(off) = find(b, prefix.as_bytes(), from) {
            from = off + 1;
            if off > 0 && b[off - 1].is_ascii_alphabetic() {
                continue;
            }
            let mut end = off + prefix.len();
            while end < b.len() && (b[end].is_ascii_alphanumeric() || matches!(b[end], b'-' | b'_'))
            {
                end += 1;
            }
            if looks_like_secret(&String::from_utf8_lossy(&b[off + prefix.len()..end])) {
                return true;
            }
        }
    }
    false
}

fn has_credential_url(b: &[u8]) -> bool {
    let mut from = 0;
    while let Some(off) = find(b, b"://", from) {
        from = off + 3;
        let mut end = from;
        while end < b.len() && !is_url_end(b[end]) {
            end += 1;
        }
        let authority = &b[from..end];
        if let Some(at) = authority.iter().rposition(|c| *c == b'@') {
            if let Some(colon) = authority[..at].iter().position(|c| *c == b':') {
                if !is_placeholder(&String::from_utf8_lossy(&authority[colon + 1..at])) {
                    return true;
                }
            }
        }
    }
    false
}

fn is_url_end(c: u8) -> bool {
    matches!(
        c,
        b' ' | b'\t'
            | b'\n'
            | b'\r'
            | b'"'
            | b'\''
            | b'`'
            | b'<'
            | b'>'
            | b')'
            | b'('
            | b']'
            | b'['
            | b'}'
            | b'{'
            | b','
            | b';'
    )
}

fn has_credential_assignment(b: &[u8]) -> bool {
    for key in ASSIGN_KEYS {
        let k = key.as_bytes();
        let mut from = 0;
        while let Some(off) = find(b, k, from) {
            from = off + 1;
            if off > 0 && b[off - 1].is_ascii_alphanumeric() {
                continue;
            }
            let mut j = off + k.len();
            if j < b.len() && b[j].is_ascii_alphanumeric() {
                continue;
            }
            while j < b.len() && matches!(b[j], b'"' | b'\'' | b'`' | b' ' | b'\t') {
                j += 1;
            }
            let mut sep = j;
            while sep < b.len() && matches!(b[sep], b'=' | b':' | b'>' | b'-') {
                sep += 1;
            }
            let sep_raw = &b[j..sep];
            if sep_raw.is_empty()
                || sep_raw.len() > 2
                || !sep_raw.iter().any(|c| *c == b'=' || *c == b':')
            {
                continue;
            }
            let mut v = sep;
            while v < b.len() && matches!(b[v], b' ' | b'\t') {
                v += 1;
            }
            if v < b.len() && matches!(b[v], b'"' | b'\'' | b'`') {
                v += 1;
            }
            let start = v;
            while v < b.len() && !is_value_end(b[v]) {
                v += 1;
            }
            if !is_placeholder(&String::from_utf8_lossy(&b[start..v])) {
                return true;
            }
        }
    }
    false
}

fn is_value_end(c: u8) -> bool {
    is_url_end(c) || matches!(c, b'@' | b'|')
}
fn looks_like_secret(v: &str) -> bool {
    !is_placeholder(v) && v.len() >= MIN_SECRET_LEN + 12 && distinct_alnum(v) >= 8
}

fn distinct_alnum(v: &str) -> usize {
    let mut seen: Vec<u8> = v.bytes().filter(|c| c.is_ascii_alphanumeric()).collect();
    seen.sort_unstable();
    seen.dedup();
    seen.len()
}

/// True when `raw` looks like a placeholder, a code reference, or too short to matter.
fn is_placeholder(raw: &str) -> bool {
    let v = raw.trim_matches(|c: char| matches!(c, '.' | ',' | ';' | ':' | '!' | '?'));
    if v.len() < MIN_SECRET_LEN {
        return true;
    }
    let lower = v.to_ascii_lowercase();
    if PLACEHOLDERS.contains(&lower.as_str()) || MARKERS.iter().any(|m| lower.contains(m)) {
        return true;
    }
    if let Some(first) = v.chars().next() {
        if v.chars().all(|c| c == first) {
            return true;
        }
    }
    // Anything outside a plausible secret alphabet (spaces, `.`, `$`, `{`, quotes, ...)
    // reads as a placeholder, a code reference or a path rather than a live literal.
    !v.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(
                c,
                '_' | '-' | '+' | '=' | '!' | '@' | '#' | '%' | '^' | '&' | '~'
            )
    })
}
#[cfg(test)]
mod tests {
    use super::check_text;

    fn rejected(text: &str) -> String {
        check_text(text)
            .expect_err(&format!("expected rejection: {text}"))
            .to_string()
    }

    #[test]
    fn allows_discussion_and_placeholders() {
        for text in [
            "",
            "Never commit your api_key or password into a ticket; rotate them often.",
            "api_key=<token>",
            "access_token: REDACTED",
            "password = ***",
            "client_secret=example",
            r#"{"api_key": "your-api-key-here"}"#,
            "api_key = process.env.OPENAI_API_KEY",
            "password: ${DB_PASSWORD}",
            "token = os.getenv(\"GITHUB_TOKEN\")",
            "The `secret` field is optional and defaults to empty.",
            "https://github.com/kroma/kroma",
            "https://user:password@example.com/x",
        ] {
            assert!(check_text(text).is_ok(), "should allow: {text}");
        }
    }

    #[test]
    fn rejects_pem_private_keys() {
        for text in [
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEow==\n-----END RSA PRIVATE KEY-----",
            "-----BEGIN OPENSSH PRIVATE KEY-----\nabc",
            "-----BEGIN ENCRYPTED PRIVATE KEY-----",
        ] {
            rejected(text);
        }
    }

    #[test]
    fn rejects_vendor_tokens_without_echoing_them() {
        let openai = "sk-proj-Kd93mQ7tLx0pZa2Bc4De6Fg8Hi1Jk3Mn5Op";
        let anthropic = "sk-ant-api03-Zq7Wm2Xc9Vb4Nt6Ry8Lu0Kp3Jh5Gf1Ds";
        let github = "ghp_9fK2mQ7xL0pZa4Bc6De8Fg1Hi3Jk5Mn7Op2Q";
        let fine_grained = "github_pat_11ABCDEFG0aBcDeFgHiJkL_9zY8xW7vU6tS5rQ4pO3nM2lK1j";
        for (text, secret) in [
            (format!("here: {openai}"), openai),
            (format!("key={anthropic}"), anthropic),
            (format!("token {github}"), github),
            (fine_grained.to_string(), fine_grained),
        ] {
            let msg = rejected(&text);
            assert!(!msg.contains(secret), "must not echo secret: {msg}");
            assert!(!msg.contains("sk-") && !msg.contains("ghp_") && !msg.contains("gh_"));
        }
        assert!(check_text("sk-short").is_ok());
        assert!(check_text("sk-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx").is_ok());
        assert!(check_text("a task-management-framework note").is_ok());
    }

    #[test]
    fn rejects_credential_urls() {
        let msg = rejected("connect to postgres://app:hunter2pass@db.internal:5432/app");
        assert!(!msg.contains("hunter2pass"));
    }

    #[test]
    fn rejects_credential_assignments() {
        let secret = "hunter2hunter2";
        let msg = rejected(&format!("password = \"{secret}\""));
        assert!(!msg.contains(secret), "must not echo: {msg}");
        for text in [
            "access_token: Kd93mQ7tLx0pZa2Bc4De",
            "{\"client_secret\": \"Zq7Wm2Xc9Vb4Nt6Ry8\"}",
            "MY_API_KEY=AbC123dEf456GhI789",
            "secret := 'a1b2c3d4e5f6g7h8'",
            "Password: Tr0ub4dor",
        ] {
            rejected(text);
        }
    }
}
