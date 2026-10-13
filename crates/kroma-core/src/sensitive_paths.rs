use std::path::{Component, Path};

/// Default sensitive credential and key path guard (§6.1).
#[derive(Debug, Clone, Default)]
pub struct SensitivePathGuard;

impl SensitivePathGuard {
    pub fn new() -> Self {
        Self
    }

    pub fn is_sensitive(path: &Path) -> bool {
        let comps: Vec<String> = path
            .components()
            .filter_map(|c| match c {
                Component::Normal(s) => Some(s.to_string_lossy().to_ascii_lowercase()),
                _ => None,
            })
            .collect();

        // Check directory components for .aws, .ssh, .kube, .gnupg, .docker, and .git/hooks or .git/config
        for (i, s) in comps.iter().enumerate() {
            if matches!(s.as_str(), ".aws" | ".ssh" | ".kube" | ".gnupg" | ".docker") {
                return true;
            }
            if s == ".git" {
                if let Some(next) = comps.get(i + 1) {
                    if matches!(next.as_str(), "hooks" | "config" | "credentials") {
                        return true;
                    }
                }
            }
        }

        let file_name = match path.file_name() {
            Some(f) => f.to_string_lossy().to_ascii_lowercase(),
            None => return false,
        };

        // .env, .envrc, .env.*, and *.env (exempt example/sample/template/test variants)
        if matches!(file_name.as_str(), ".env" | ".envrc") {
            return true;
        }
        if file_name.starts_with(".env.") {
            if matches!(
                file_name.as_str(),
                ".env.example" | ".env.sample" | ".env.template" | ".env.test"
            ) {
                return false;
            }
            return true;
        }
        if file_name.ends_with(".env") {
            if matches!(
                file_name.as_str(),
                "example.env" | "sample.env" | "template.env" | "test.env"
            ) {
                return false;
            }
            return true;
        }

        // Private keys & certificates
        if file_name.ends_with(".pem")
            || file_name.ends_with(".key")
            || file_name.ends_with(".p12")
            || file_name.ends_with(".pfx")
            || file_name.ends_with(".jks")
            || file_name.ends_with(".keystore")
            || file_name.starts_with("id_rsa")
            || file_name.starts_with("id_ed25519")
            || file_name.starts_with("id_ecdsa")
            || file_name.starts_with("id_dsa")
        {
            return true;
        }

        // Credential & state files
        if matches!(
            file_name.as_str(),
            ".git-credentials" | ".npmrc" | ".pypirc" | ".netrc" | "_netrc" | ".pgpass" | "credentials" | "credentials.toml"
        ) {
            return true;
        }
        if file_name.contains("credentials") && (file_name.ends_with(".json") || file_name.ends_with(".toml")) {
            return true;
        }
        if file_name.starts_with("terraform.tfstate") || file_name.ends_with(".tfvars") {
            return true;
        }

        false
    }
}
