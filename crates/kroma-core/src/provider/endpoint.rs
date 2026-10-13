//! Provider base-URL validation shared by every wire protocol.
//!
//! Kept in its own module so the Chat Completions, Anthropic Messages and OpenAI Responses clients
//! apply byte-identical security rules: HTTPS for remote hosts, plain HTTP only for loopback, no
//! userinfo, no query/fragment, no control bytes.

use crate::provider::ProviderError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedEndpoint {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub base_path: String,
}

impl ValidatedEndpoint {
    /// `Host:` header value (omits the default port for the scheme).
    pub fn host_header(&self) -> String {
        let is_default_port = (self.scheme == "http" && self.port == 80)
            || (self.scheme == "https" && self.port == 443);
        if is_default_port {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    pub fn connect_addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// Validate `base_url` security rules (§4.3):
/// - Rejects any URL containing userinfo (`user:pass@host`).
/// - Enforces HTTPS for remote hosts; allows HTTP ONLY for `127.0.0.1`, `::1`, `[::1]`, `localhost`.
pub fn validate_base_url(raw_url: &str) -> Result<ValidatedEndpoint, ProviderError> {
    // F-04: the URL is split by hand and its path is interpolated directly into a hand-written
    // HTTP request line, so reject every control byte and whitespace character up front.
    if let Some(bad) = raw_url
        .bytes()
        .find(|b| *b < 0x20 || *b == 0x7f || *b == b' ' || *b == b'\t')
    {
        return Err(ProviderError::InsecureUrl(format!(
            "URL contains a forbidden control or whitespace byte (0x{:02x})",
            bad
        )));
    }
    let trimmed = raw_url.trim();
    let (scheme, rest) = if let Some(r) = trimmed.strip_prefix("https://") {
        ("https", r)
    } else if let Some(r) = trimmed.strip_prefix("http://") {
        ("http", r)
    } else {
        return Err(ProviderError::InsecureUrl(format!(
            "URL '{}' must start with https:// or http://",
            raw_url
        )));
    };

    if trimmed.contains('?') || trimmed.contains('#') {
        return Err(ProviderError::InsecureUrl(
            "URL must not contain query ('?') or fragment ('#')".to_string(),
        ));
    }

    let (authority, path_part) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, ""),
    };

    if authority.contains('@') {
        return Err(ProviderError::InsecureUrl(
            "URL containing userinfo ('user:pass@host') is forbidden".to_string(),
        ));
    }

    let (host, port) = parse_host_port(authority, if scheme == "https" { 443 } else { 80 })?;

    if scheme == "http" && !is_loopback_host(&host) {
        return Err(ProviderError::InsecureUrl(format!(
            "plain HTTP is only permitted for localhost/127.0.0.1/::1, rejected remote host '{}'",
            host
        )));
    }

    let clean_path = path_part.trim_end_matches('/').to_string();

    Ok(ValidatedEndpoint {
        scheme: scheme.to_string(),
        host,
        port,
        base_path: clean_path,
    })
}

fn is_loopback_host(host: &str) -> bool {
    matches!(
        host.to_ascii_lowercase().as_str(),
        "localhost" | "127.0.0.1" | "::1" | "[::1]"
    )
}

fn parse_host_port(authority: &str, default_port: u16) -> Result<(String, u16), ProviderError> {
    if authority.is_empty() {
        return Err(ProviderError::InsecureUrl("empty host".to_string()));
    }
    if authority.starts_with('[') {
        // IPv6 literal `[::1]:8080`
        let close = authority
            .find(']')
            .ok_or_else(|| ProviderError::InsecureUrl("unclosed IPv6 bracket".to_string()))?;
        let host = &authority[..=close];
        let after = &authority[close + 1..];
        if after.is_empty() {
            return Ok((host.to_string(), default_port));
        }
        if let Some(p_str) = after.strip_prefix(':') {
            let p = p_str
                .parse::<u16>()
                .map_err(|_| ProviderError::InsecureUrl("invalid port".to_string()))?;
            return Ok((host.to_string(), p));
        }
        return Err(ProviderError::InsecureUrl("invalid authority".to_string()));
    }

    if let Some((h, p_str)) = authority.rsplit_once(':') {
        let p = p_str
            .parse::<u16>()
            .map_err(|_| ProviderError::InsecureUrl("invalid port".to_string()))?;
        Ok((h.to_string(), p))
    } else {
        Ok((authority.to_string(), default_port))
    }
}

