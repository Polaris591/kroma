//! Explicit, read-only version checks. Native executables never replace themselves.
use anyhow::{bail, Context, Result};
use semver::Version;
use serde::Deserialize;
use std::time::Duration;

const REGISTRY: &str = "https://registry.npmjs.org/kroma";
const MAX_METADATA_BYTES: usize = 1024 * 1024;

#[derive(Deserialize)]
struct Release {
    name: String,
    version: Version,
}

pub async fn run(check: bool, channel: &str) -> Result<()> {
    if !check {
        crate::cli_println!(
            "This native executable cannot update itself. No installation was changed."
        );
        crate::cli_println!(
            "Source build: git pull --ff-only, then cargo build --release --locked."
        );
        crate::cli_println!(
            "Cargo install: repeat your original cargo install command with --force."
        );
        crate::cli_println!("npm install: invoke the npm-installed kroma launcher, or npm install --global kroma@{channel}.");
        crate::cli_println!("Use kroma update --check to query the published version.");
        return Ok(());
    }
    let current = Version::parse(env!("CARGO_PKG_VERSION"))?;
    let latest = published_version(REGISTRY, channel).await?;
    crate::cli_println!("Installed: {current}; npm {channel}: {latest}");
    if latest.cmp_precedence(&current).is_gt() {
        crate::cli_println!(
            "An update is available. Repeat your original installation method to upgrade."
        );
    } else {
        crate::cli_println!("No newer version on this channel. Nothing was changed.");
    }
    Ok(())
}

async fn published_version(registry: &str, channel: &str) -> Result<Version> {
    if !matches!(channel, "latest" | "next") {
        bail!("update channel must be latest or next");
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("kroma/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let mut response = client
        .get(format!("{registry}/{channel}"))
        .send()
        .await
        .context("checking the npm registry; no installation was changed")?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        bail!("no published kroma version on npm channel '{channel}'; nothing was changed");
    }
    if !response.status().is_success() {
        bail!(
            "npm version check returned {}; nothing was changed",
            response.status()
        );
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len().saturating_add(chunk.len()) > MAX_METADATA_BYTES {
            bail!("npm metadata exceeds the size limit");
        }
        bytes.extend_from_slice(&chunk);
    }
    let release: Release =
        serde_json::from_slice(&bytes).context("invalid npm version metadata")?;
    if release.name != "kroma" {
        bail!("npm metadata is not for kroma");
    }
    if channel == "latest" && !release.version.pre.is_empty() {
        bail!("npm latest unexpectedly points to a prerelease; use --channel next explicitly");
    }
    Ok(release.version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    async fn lookup(status: &str, body: &str) -> Result<Version> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            let count = socket.read(&mut request).unwrap();
            assert!(String::from_utf8_lossy(&request[..count]).starts_with("GET /latest "));
            let _ = socket.write_all(response.as_bytes());
        });
        let result = published_version(&format!("http://{addr}"), "latest").await;
        server.join().unwrap();
        result
    }

    #[tokio::test]
    async fn version_checks_validate_metadata_without_model_configuration() {
        assert_eq!(
            lookup("200 OK", r#"{"name":"kroma","version":"1.2.3"}"#)
                .await
                .unwrap(),
            Version::new(1, 2, 3)
        );
        for body in [
            r#"{"name":"other","version":"1.2.3"}"#,
            r#"{"name":"kroma","version":"invalid"}"#,
            r#"{"name":"kroma","version":"1.2.3-beta.1"}"#,
            "not json",
        ] {
            assert!(lookup("200 OK", body).await.is_err());
        }
        assert!(lookup("404 Not Found", "{}")
            .await
            .unwrap_err()
            .to_string()
            .contains("no published"));
        assert!(lookup("302 Found", "{}").await.is_err());
    }

    #[tokio::test]
    async fn oversized_metadata_is_rejected() {
        assert!(lookup("200 OK", &"x".repeat(MAX_METADATA_BYTES + 1))
            .await
            .is_err());
    }
}
