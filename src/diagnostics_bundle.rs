//! Explicit, local-only diagnostics bundle.
//!
//! The report is built from a small allowlist. It never serializes provider
//! credentials, environment variables, plugin settings, conversation content,
//! tool inputs/outputs, or credential command results. It never starts a
//! plugin process or makes a model request.
use crate::{
    config::Config,
    model_catalog::{ModelReport, CATALOG_SCHEMA_VERSION},
    plugins::PluginCatalog,
};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::{
    env, fs,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub const FORMAT_VERSION: u32 = 1;
const MAX_ERROR_CHARS: usize = 500;

const README: &str = "\
Kroma local diagnostics
=======================

This directory was generated locally. Kroma did not upload it or contact a
model provider, and it did not start any plugin process.

Included by allowlist:
- Kroma version, build revision, and executable path
- OS, architecture, and working directory
- effective model limits and their sources (configured/catalog/default only;
  upstream metadata was not queried)
- plugin IDs, versions, enabled state, API/protocol versions
- bounded configuration/plugin-discovery errors and a plugin warning count
  (raw warning text is omitted because it can quote settings)

Not included:
- API keys, auth/cookie headers (including bearer tokens), or other credentials
- environment variable values
- conversation/session messages
- tool inputs or outputs
- plugin settings
- source file contents

Paths are home-redacted when they are under HOME/USERPROFILE. Other local
paths may still identify a machine or user; review before sharing. This is a
best-effort structural allowlist, not a promise that arbitrary plugin warning
text can never contain sensitive data. Session excerpts are not supported in
this batch.
";

#[derive(Debug, Clone, Serialize)]
pub struct BundleReport {
    pub format_version: u32,
    pub catalog_schema_version: u32,
    pub generated_at_unix: u64,
    pub kroma: BuildInfo,
    pub host: HostInfo,
    pub workdir: String,
    pub config: Option<ConfigSummary>,
    pub errors: Vec<String>,
    pub plugins: Vec<PluginSummary>,
    /// Count only: discovery warning text can quote plugin.toml/plugins.toml
    /// content, so it is not included in a default bundle.
    pub plugin_warning_count: usize,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BuildInfo {
    pub version: &'static str,
    pub build_revision: &'static str,
    pub executable: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct HostInfo {
    pub os: &'static str,
    pub arch: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigSummary {
    pub model: String,
    pub provider_host: String,
    pub protocol: String,
    pub reasoning_effort: String,
    pub deepseek_thinking: String,
    pub selected_profile: Option<String>,
    pub api_key_present: bool,
    pub model_limits: ModelReport,
}

impl ConfigSummary {
    fn from_config(config: &Config) -> Result<Self> {
        let provider_host = reqwest::Url::parse(&config.base_url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
            .unwrap_or_else(|| "(invalid)".into());
        let (_, model_limits) = config.resolved_limits_without_probe()?;
        Ok(Self {
            model: config.model.clone(),
            provider_host,
            protocol: config.api_protocol.clone(),
            reasoning_effort: config.reasoning_effort.clone(),
            deepseek_thinking: config
                .deepseek_thinking
                .clone()
                .unwrap_or_else(|| "off".into()),
            selected_profile: config.selected_profile.clone(),
            api_key_present: config.api_key.is_some(),
            model_limits,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginSummary {
    pub id: String,
    pub name: String,
    pub version: String,
    pub enabled: bool,
    pub transient: bool,
    pub project: bool,
    pub api_version: u64,
    pub protocol_version: u64,
    pub has_model: bool,
    pub has_context: bool,
    pub has_ui: bool,
}

impl PluginSummary {
    /// Copy only allowlisted fields from the existing structured summary.
    fn from_value(value: &Value) -> Self {
        let string = |key: &str| value[key].as_str().unwrap_or("").to_owned();
        let flag = |key: &str| value[key].as_bool().unwrap_or(false);
        let number = |key: &str| value[key].as_u64().unwrap_or(0);
        Self {
            id: string("id"),
            name: string("name"),
            version: string("version"),
            enabled: flag("enabled"),
            transient: flag("transient"),
            project: flag("project"),
            api_version: number("api_version"),
            protocol_version: number("protocol_version"),
            has_model: flag("has_model"),
            has_context: flag("has_context"),
            has_ui: flag("has_ui"),
        }
    }
}

/// Generate a local bundle and return its directory. An existing target is
/// never overwritten. `None` chooses a timestamped directory under the workdir.
pub fn generate(output: Option<PathBuf>) -> Result<PathBuf> {
    let config = Config::from_env();
    let workdir = match &config {
        Ok(config) => config.workdir.clone(),
        Err(_) => env::current_dir()
            .context("working directory is unavailable")?
            .canonicalize()
            .context("working directory does not exist")?,
    };
    let mut errors = Vec::new();
    let config_summary = match &config {
        Ok(config) => match ConfigSummary::from_config(config) {
            Ok(summary) => Some(summary),
            Err(error) => {
                errors.push(bounded_error("effective model limits", &error));
                None
            }
        },
        Err(error) => {
            errors.push(bounded_error("configuration", error));
            None
        }
    };
    let (plugins, plugin_warning_count) = match PluginCatalog::discover(&workdir) {
        Ok(catalog) => {
            let warning_count = catalog.warnings.len();
            match catalog.summaries() {
                Ok(summaries) => (
                    summaries
                        .iter()
                        .map(PluginSummary::from_value)
                        .collect::<Vec<_>>(),
                    warning_count,
                ),
                Err(error) => {
                    errors.push(bounded_error("plugin summaries", &error));
                    (Vec::new(), warning_count)
                }
            }
        }
        Err(error) => {
            errors.push(bounded_error("plugin discovery", &error));
            (Vec::new(), 0)
        }
    };
    let report = BundleReport {
        format_version: FORMAT_VERSION,
        catalog_schema_version: CATALOG_SCHEMA_VERSION,
        generated_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        kroma: BuildInfo {
            version: env!("CARGO_PKG_VERSION"),
            build_revision: env!("KROMA_BUILD_REVISION"),
            executable: current_exe_redacted(),
        },
        host: HostInfo {
            os: env::consts::OS,
            arch: env::consts::ARCH,
        },
        workdir: redact_path(&workdir),
        config: config_summary,
        errors,
        plugins,
        plugin_warning_count,
        notes: vec![
            "local-only; no upload, no model request, no plugin process".into(),
            "credentials, environment values, plugin settings and conversation content are excluded".into(),
            "raw plugin discovery warning text is omitted; only its count is included".into(),
            "session excerpts are not supported in this diagnostics format".into(),
        ],
    };
    let target = output.unwrap_or_else(|| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        workdir.join(format!("kroma-diagnostics-{nanos:x}"))
    });
    write_bundle(&target, &report)
}

fn write_bundle(target: &Path, report: &BundleReport) -> Result<PathBuf> {
    if let Some(parent) = target.parent().filter(|path| !path.as_os_str().is_empty()) {
        fs::create_dir_all(parent).context("creating diagnostics parent directory")?;
    }
    match fs::create_dir(target) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            bail!(
                "diagnostics target already exists; refusing to overwrite {}",
                target.display()
            )
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("creating diagnostics directory {}", target.display()))
        }
    }
    let summary = serde_json::to_vec_pretty(report)?;
    if let Err(error) = write_new(&target.join("summary.json"), &summary)
        .and_then(|()| write_new(&target.join("README.txt"), README.as_bytes()))
    {
        let _ = fs::remove_dir_all(target);
        return Err(error);
    }
    Ok(target.to_path_buf())
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .with_context(|| format!("writing {}", path.display()))
}

fn bounded_error(label: &str, error: &anyhow::Error) -> String {
    truncate(&format!("{label}: {error:#}"), MAX_ERROR_CHARS)
}

fn truncate(text: &str, limit: usize) -> String {
    let mut chars = text.chars();
    let result: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        format!("{result}...")
    } else {
        result
    }
}

fn current_exe_redacted() -> String {
    env::current_exe()
        .map(|path| redact_path(&path))
        .unwrap_or_else(|_| "(unavailable)".into())
}

fn redact_path(path: &Path) -> String {
    let home = env::var_os("HOME").or_else(|| env::var_os("USERPROFILE"));
    if let Some(home) = home {
        let home = Path::new(&home);
        if let Ok(rest) = path.strip_prefix(home) {
            if rest.as_os_str().is_empty() {
                return "~".into();
            }
            return format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display());
        }
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_catalog::FieldSource;

    fn test_config() -> Config {
        Config {
            base_url: "https://api.deepseek.com".into(),
            api_key: Some("sk-test-DO-NOT-EMIT".into()),
            model: "deepseek-flash".into(),
            workdir: env::temp_dir(),
            deepseek_thinking: None,
            api_protocol: "chat".into(),
            reasoning_effort: "auto".into(),
            max_output_tokens: None,
            max_output_tokens_source: None,
            context_window: None,
            context_window_source: None,
            thinking_levels: Vec::new(),
            thinking_levels_source: None,
            catalog_model: None,
            catalog_model_source: None,
            selected_profile: None,
        }
    }

    #[test]
    fn default_bundle_is_allowlisted_and_never_overwrites() -> Result<()> {
        let root = env::temp_dir().join(format!(
            "kroma-diagnostics-test-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir_all(&root)?;
        let config = test_config();
        let summary = ConfigSummary::from_config(&config)?;
        assert_eq!(
            summary.model_limits.context_window.source,
            FieldSource::Catalog
        );
        assert!(summary.api_key_present);
        let report = BundleReport {
            format_version: FORMAT_VERSION,
            catalog_schema_version: CATALOG_SCHEMA_VERSION,
            generated_at_unix: 1,
            kroma: BuildInfo {
                version: "test",
                build_revision: "test",
                executable: "~/kroma".into(),
            },
            host: HostInfo {
                os: "test",
                arch: "test",
            },
            workdir: "~/project".into(),
            config: Some(summary),
            errors: vec!["bounded test error".into()],
            plugins: vec![PluginSummary {
                id: "example".into(),
                name: "Example".into(),
                version: "0.1".into(),
                enabled: true,
                transient: true,
                project: false,
                api_version: 1,
                protocol_version: 1,
                has_model: false,
                has_context: false,
                has_ui: false,
            }],
            plugin_warning_count: 0,
            notes: vec!["test".into()],
        };
        let target = root.join("bundle");
        write_bundle(&target, &report)?;
        let text = fs::read_to_string(target.join("summary.json"))?;
        assert!(text.contains("\"api_key_present\": true"));
        assert!(!text.contains("sk-test-DO-NOT-EMIT"));
        assert!(!text.to_ascii_lowercase().contains("authorization"));
        assert!(!text.to_ascii_lowercase().contains("cookie"));
        assert!(!text.contains("\"messages\""));
        assert!(!text.contains("tool_call"));
        assert!(!text.contains("api_key\""));
        assert!(write_bundle(&target, &report).is_err());
        assert!(target.join("summary.json").is_file());
        fs::remove_dir_all(root)?;
        Ok(())
    }
}
