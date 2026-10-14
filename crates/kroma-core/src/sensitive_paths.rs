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

        // Check directory components for the credential/key dot-directories. Any component named
        // `.git` marks the whole subtree sensitive: git internals are not workspace content and the
        // file tools (read/write/edit/grep/find/ls) must never touch them. Git state that the agent
        // legitimately needs is reached through the `bash` tool, not through this guard. Blocking
        // only `.git/hooks|config|credentials` left `.git/modules/<sub>/hooks/*` (submodule hook
        // execution), `.git/info/attributes` (malicious `filter=` drivers) and `.git/HEAD` /
        // `.git/refs/*` writable. `to_ascii_lowercase` above already folds Windows `.GIT`.
        for s in &comps {
            if matches!(
                s.as_str(),
                ".aws" | ".ssh" | ".kube" | ".gnupg" | ".docker" | ".git"
            ) {
                return true;
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
