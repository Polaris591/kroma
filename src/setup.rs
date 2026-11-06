//! Guided first-run model setup. It never writes a profile until the user
//! confirms, and optionally verifies a real completion first.
use crate::{
    app, model_catalog,
    profiles::{self, Profiles, ProviderProfile},
};
use anyhow::{bail, Context, Result};
use std::{
    env,
    io::{self, IsTerminal, Write},
    time::Instant,
};

struct Preset {
    key: &'static str,
    label: &'static str,
    base_url: &'static str,
    protocol: &'static str,
    catalog_provider: Option<&'static str>,
    key_env: Option<&'static str>,
}

const PRESETS: &[Preset] = &[
    Preset {
        key: "openai",
        label: "OpenAI",
        base_url: "https://api.openai.com/v1",
        protocol: "chat",
        catalog_provider: Some("openai"),
        key_env: Some("OPENAI_API_KEY"),
    },
    Preset {
        key: "anthropic",
        label: "Anthropic",
        base_url: "https://api.anthropic.com/v1",
        protocol: "anthropic",
        catalog_provider: Some("anthropic"),
        key_env: Some("ANTHROPIC_API_KEY"),
    },
    Preset {
        key: "deepseek",
        label: "DeepSeek",
        base_url: "https://api.deepseek.com/v1",
        protocol: "chat",
        catalog_provider: Some("deepseek"),
        key_env: Some("DEEPSEEK_API_KEY"),
    },
    Preset {
        key: "ollama",
        label: "Ollama (local)",
        base_url: "http://127.0.0.1:11434/v1",
        protocol: "chat",
        catalog_provider: None,
        key_env: None,
    },
    Preset {
        key: "custom",
        label: "Custom OpenAI/Anthropic-compatible endpoint",
        base_url: "",
        protocol: "chat",
        catalog_provider: None,
        key_env: None,
    },
];

fn prompt(label: &str) -> Result<String> {
    crate::cli_print!("{label}");
    io::stdout().flush()?;
    let mut value = String::new();
    if io::stdin().read_line(&mut value)? == 0 {
        bail!("setup cancelled (stdin closed)");
    }
    Ok(value.trim().to_owned())
}

fn prompt_default(label: &str, default: &str) -> Result<String> {
    crate::cli_print!("{label} [{default}]: ");
    io::stdout().flush()?;
    let mut value = String::new();
    if io::stdin().read_line(&mut value)? == 0 {
        bail!("setup cancelled (stdin closed)");
    }
    let value = value.trim();
    Ok(if value.is_empty() {
        default.to_owned()
    } else {
        value.to_owned()
    })
}

fn yes(label: &str, default_yes: bool) -> Result<bool> {
    let suffix = if default_yes { "[Y/n]" } else { "[y/N]" };
    let value = prompt(&format!("{label} {suffix}: "))?;
    if value.is_empty() {
        return Ok(default_yes);
    }
    match value.to_ascii_lowercase().as_str() {
        "y" | "yes" => Ok(true),
        "n" | "no" => Ok(false),
        _ => yes(label, default_yes),
    }
}

fn choose_model(provider: Option<&'static str>) -> Result<String> {
    if let Some(provider) = provider {
        let suggestions = model_catalog::suggestions(provider, 8);
        if !suggestions.is_empty() {
            crate::cli_println!("\nSuggested model IDs (context capacity):");
            for (index, record) in suggestions.iter().enumerate() {
                let context = record
                    .context_window
                    .map(|value| format!("{}k", value / 1000))
                    .unwrap_or_else(|| "?".into());
                crate::cli_println!("  {}. {:<40} {}", index + 1, record.model, context);
            }
            let default = suggestions[0].model;
            let value = prompt_default("Choose a number or type an exact model ID", default)?;
            if let Ok(index) = value.parse::<usize>() {
                if let Some(record) = index
                    .checked_sub(1)
                    .and_then(|index| suggestions.get(index))
                {
                    return Ok(record.model.to_owned());
                }
                bail!("model choice is out of range");
            }
            return Ok(value);
        }
    }
    prompt_default("Model ID", "")
}

pub async fn run() -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!(
            "kroma setup needs an interactive terminal; use environment variables \
             (KROMA_MODEL, KROMA_BASE_URL, KROMA_API_KEY) or `kroma config set <name>` on a terminal"
        );
    }
    crate::cli_println!("Kroma setup\n");
    let profiles = Profiles::load()?;
    let default_name = profiles
        .active
        .clone()
        .unwrap_or_else(|| "default".to_owned());
    let name = loop {
        let value = prompt_default("Profile name", &default_name)?;
        if profiles::valid_name(&value) {
            break value;
        }
        crate::cli_println!("Profile names use letters, digits, '-' or '_'.");
    };
    let original = profiles.profiles.get(&name).cloned().unwrap_or_default();

    crate::cli_println!("\nProvider:");
    for (index, preset) in PRESETS.iter().enumerate() {
        crate::cli_println!("  {}. {}", index + 1, preset.label);
    }
    let choice = loop {
        let value = prompt_default("Choose a provider", "1")?;
        if let Ok(index) = value.parse::<usize>() {
            if (1..=PRESETS.len()).contains(&index) {
                break index;
            }
        }
        crate::cli_println!("Choose 1-{}.", PRESETS.len());
    };
    let preset = &PRESETS[choice - 1];

    let base_url = if preset.base_url.is_empty() {
        let value = prompt_default(
            "API base URL",
            if original.base_url.is_empty() {
                "https://api.openai.com/v1"
            } else {
                &original.base_url
            },
        )?;
        profiles::normalize_endpoint(&value)?
    } else {
        preset.base_url.to_owned()
    };
    let protocol = if preset.key == "custom" {
        let value = prompt_default("API protocol (chat/responses/anthropic)", "chat")?;
        profiles::normalize_protocol(&value)?
    } else {
        preset.protocol.to_owned()
    };

    let model = choose_model(preset.catalog_provider)?;
    if model.trim().is_empty() {
        bail!("model ID cannot be empty");
    }

    let mut api_key = None;
    let mut api_key_env = None;
    if let Some(env_name) = preset.key_env.filter(|name| env::var_os(name).is_some()) {
        if yes(
            &format!("Use {env_name} from the environment instead of storing a key?"),
            true,
        )? {
            api_key_env = Some(env_name.to_owned());
        }
    }
    if api_key_env.is_none() {
        crate::cli_println!(
            "\nAPI key is hidden while typing. Leave it empty for a gateway that needs no key."
        );
        if let Some(key) = profiles::prompt_key(false)? {
            if !key.is_empty() {
                api_key = Some(key);
            }
        }
    }

    let profile = ProviderProfile {
        base_url: base_url.clone(),
        model: model.clone(),
        api_key,
        api_key_env,
        deepseek_thinking: original.deepseek_thinking.clone(),
        api_protocol: Some(protocol.clone()),
        reasoning_effort: Some(
            original
                .reasoning_effort
                .clone()
                .unwrap_or_else(|| "auto".into()),
        ),
        context_window: original.context_window,
        max_output_tokens: original.max_output_tokens,
        thinking_levels: original.thinking_levels.clone(),
        catalog_model: original.catalog_model.clone(),
        native_web_search: original.native_web_search,
    };

    let mut save = true;
    if yes("Test this connection with a tiny completion now?", true)? {
        crate::cli_print!("Testing {} at {}... ", model, base_url);
        io::stdout().flush()?;
        let started = Instant::now();
        match app::model_from_profile(&profile).await {
            Ok((backend, _, _)) => {
                match backend
                    .generate_short_text("You are a connectivity test. Reply with OK.", "ping", 16)
                    .await
                {
                    Ok(result) => {
                        crate::cli_println!("ok in {:?}", started.elapsed());
                        if !result.text.trim().is_empty() {
                            crate::cli_println!("Reply: {}", result.text.trim());
                        }
                    }
                    Err(error) => {
                        crate::cli_println!("failed");
                        eprintln!("Error: {error:#}");
                        save = yes("Save anyway (the endpoint may reject test prompts)?", false)?;
                    }
                }
            }
            Err(error) => {
                crate::cli_println!("failed");
                eprintln!("Error: {error:#}");
                save = yes("Save anyway (the endpoint may be unavailable now)?", false)?;
            }
        }
    }
    if !save {
        bail!("setup cancelled; no profile was saved");
    }

    Profiles::update(|profiles| {
        profiles.profiles.insert(name.clone(), profile);
        profiles.active = Some(name.clone());
        Ok(())
    })
    .with_context(|| format!("saving profile '{name}'"))?;
    crate::cli_println!("\nSaved profile '{name}'.");
    Ok(())
}
