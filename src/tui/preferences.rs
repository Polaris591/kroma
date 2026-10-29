//! UI-only preferences. Never change model prompts or saved conversation text.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(super) enum Language {
    #[default]
    En,
    Zh,
}

impl Language {
    pub fn text<'a>(self, en: &'a str, zh: &'a str) -> &'a str {
        match self {
            Self::En => en,
            Self::Zh => zh,
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "en" | "english" | "English" => Ok(Self::En),
            "zh" | "zh-CN" | "中文" => Ok(Self::Zh),
            _ => bail!("/language [en|zh]"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub(super) struct Preferences {
    pub language: Language,
    pub full_width: bool,
}

impl Preferences {
    pub fn path() -> Option<PathBuf> {
        env::var_os("HOME")
            .or_else(|| env::var_os("USERPROFILE"))
            .map(|home| PathBuf::from(home).join(".kroma/ui.toml"))
    }

    pub fn load_at(path: Option<&Path>) -> Result<Self> {
        match path {
            Some(path) => match fs::read_to_string(path) {
                Ok(text) => toml::from_str(&text).context("invalid ui.toml"),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
                Err(error) => Err(error).context("reading ui.toml"),
            },
            None => Ok(Self::default()),
        }
    }

    pub fn save_at(self, path: Option<&Path>) -> Result<()> {
        let path = path.context("HOME/USERPROFILE unavailable; cannot save UI preferences")?;
        let parent = path
            .parent()
            .context("UI preferences need a parent directory")?;
        let temp = crate::private_file::save(
            parent,
            "ui",
            "tmp",
            toml::to_string_pretty(&self)?.as_bytes(),
        )?;
        if let Err(error) = crate::atomic_file::replace(&temp, path) {
            let _ = fs::remove_file(temp);
            return Err(error);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preferences_round_trip_and_reject_invalid_values() -> Result<()> {
        let root = env::temp_dir().join(format!(
            "kroma-ui-prefs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        let path = root.join("ui.toml");
        assert_eq!(Preferences::load_at(Some(&path))?, Preferences::default());
        let prefs = Preferences {
            language: Language::Zh,
            full_width: true,
        };
        prefs.save_at(Some(&path))?;
        assert_eq!(Preferences::load_at(Some(&path))?, prefs);
        fs::write(&path, "language = 'invalid'")?;
        assert!(Preferences::load_at(Some(&path)).is_err());
        assert!(toml::from_str::<Preferences>("typo = true").is_err());
        assert!(Language::parse("fr").is_err());
        fs::remove_dir_all(root)?;
        Ok(())
    }
}
