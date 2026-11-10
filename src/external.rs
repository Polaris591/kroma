//! Open explicit user-clicked HTTP(S) links with the operating system handler.
//!
//! URLs are parsed and validated first. Windows uses ShellExecuteW, the same
//! mechanism as the terminal client, instead of constructing a shell string.

use anyhow::{bail, Context, Result};

/// Parse an externally openable link. Credentials are rejected because they
/// are unnecessary for a documentation link and invite misleading UI.
pub fn validate_http_url(raw: &str) -> Result<reqwest::Url> {
    if raw.len() > 4096 || raw.chars().any(char::is_control) || raw.contains('\\') {
        bail!("only a plain http(s) link can be opened from the chat");
    }
    // `Url::parse` normalizes `https:///host` to a valid host. An explicitly
    // empty authority is almost always a typo or an attempt to confuse a
    // human about where the link goes, so reject it before parsing.
    let Some((_, after_scheme)) = raw.split_once("://") else {
        bail!("only a plain http(s) link can be opened from the chat");
    };
    if after_scheme
        .split(['/', '?', '#'])
        .next()
        .is_none_or(str::is_empty)
    {
        bail!("external URL needs a host");
    }
    let url = reqwest::Url::parse(raw).context("invalid external URL")?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("only HTTP(S) links can be opened externally");
    }
    if url.host_str().is_none_or(str::is_empty) {
        bail!("external URL needs a host");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("external URL must not contain credentials");
    }
    Ok(url)
}

pub fn open_http_url(raw: &str) -> Result<()> {
    let url = validate_http_url(raw)?;
    open_with_system(std::ffi::OsStr::new(url.as_str()))
}

#[cfg(windows)]
fn open_with_system(value: &std::ffi::OsStr) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;

    let wide: Vec<u16> = value.encode_wide().chain(std::iter::once(0)).collect();
    let verb: Vec<u16> = "open".encode_utf16().chain(std::iter::once(0)).collect();
    // ShellExecute receives data arguments, never interpolated PowerShell or
    // cmd source.
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            wide.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
        )
    } as isize;
    if result <= 32 {
        bail!("Windows could not open this link (ShellExecute code {result})");
    }
    Ok(())
}

#[cfg(not(windows))]
fn open_with_system(value: &std::ffi::OsStr) -> Result<()> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(program)
        .arg(value)
        .spawn()
        .with_context(|| format!("starting {program}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_http_links() {
        assert_eq!(
            validate_http_url("https://example.com/docs?q=1")
                .unwrap()
                .as_str(),
            "https://example.com/docs?q=1"
        );
        assert!(validate_http_url("http://127.0.0.1:8080/x").is_ok());
    }

    #[test]
    fn rejects_unsafe_or_non_http_links() {
        for raw in [
            "javascript:alert(1)",
            "file:///C:/Windows/System32/calc.exe",
            "https://user:secret@example.com/",
            r"https:\\example.com",
            "https:///missing-host",
        ] {
            assert!(validate_http_url(raw).is_err(), "{raw}");
        }
    }
}
