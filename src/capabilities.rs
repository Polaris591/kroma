//! Persistent, credential-free capability choices shared by all hosts.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CapabilityGroup {
    Tools,
    Mcp,
    Skills,
    Plugins,
}

#[derive(Clone, Default, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct CapabilityItems {
    pub tools: BTreeMap<String, bool>,
    pub mcp: BTreeMap<String, bool>,
    pub skills: BTreeMap<String, bool>,
    pub plugins: BTreeMap<String, bool>,
}
impl CapabilityItems {
    pub fn group(&self, group: CapabilityGroup) -> &BTreeMap<String, bool> {
        match group {
            CapabilityGroup::Tools => &self.tools,
            CapabilityGroup::Mcp => &self.mcp,
            CapabilityGroup::Skills => &self.skills,
            CapabilityGroup::Plugins => &self.plugins,
        }
    }
    pub fn group_mut(&mut self, group: CapabilityGroup) -> &mut BTreeMap<String, bool> {
        match group {
            CapabilityGroup::Tools => &mut self.tools,
            CapabilityGroup::Mcp => &mut self.mcp,
            CapabilityGroup::Skills => &mut self.skills,
            CapabilityGroup::Plugins => &mut self.plugins,
        }
    }
    pub fn patch(
        &mut self,
        group: CapabilityGroup,
        items: &BTreeMap<String, Option<bool>>,
        defaults: bool,
    ) -> Result<()> {
        anyhow::ensure!(
            !defaults || items.values().all(Option::is_some),
            "default item values cannot be null"
        );
        anyhow::ensure!(
            items.keys().all(|id| !id.trim().is_empty()),
            "item id cannot be empty"
        );
        let target = self.group_mut(group);
        for (id, enabled) in items {
            match enabled {
                Some(enabled) => {
                    target.insert(id.clone(), *enabled);
                }
                None => {
                    target.remove(id);
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Capabilities {
    pub tools: bool,
    pub mcp: bool,
    pub skills: bool,
    pub plugins: bool,
    pub items: CapabilityItems,
}
impl Capabilities {
    pub fn group_enabled(&self, group: CapabilityGroup) -> bool {
        match group {
            CapabilityGroup::Tools => self.tools,
            CapabilityGroup::Mcp => self.mcp,
            CapabilityGroup::Skills => self.skills,
            CapabilityGroup::Plugins => self.plugins,
        }
    }
    pub fn enabled(&self, group: CapabilityGroup, id: &str, original: bool) -> bool {
        let legacy = if group == CapabilityGroup::Tools
            && !crate::app::builtin_tool_names()
                .iter()
                .any(|name| name == id)
        {
            true
        } else {
            self.group_enabled(group)
        };
        self.items
            .group(group)
            .get(id)
            .copied()
            .unwrap_or(legacy && original)
    }
    pub fn any_enabled(&self, group: CapabilityGroup) -> bool {
        self.group_enabled(group) || self.items.group(group).values().any(|enabled| *enabled)
    }
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            tools: true,
            mcp: true,
            skills: true,
            plugins: true,
            items: CapabilityItems::default(),
        }
    }
}

#[derive(Clone, Default, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct CapabilityOverrides {
    pub tools: Option<bool>,
    pub mcp: Option<bool>,
    pub skills: Option<bool>,
    pub plugins: Option<bool>,
    pub items: CapabilityItems,
}

impl CapabilityOverrides {
    /// Convert legacy category choices into visible item overrides before an
    /// item edit. Removing one of these overrides then really inherits defaults.
    pub fn expand_legacy(&mut self, workdir: &Path) -> Result<()> {
        if let Some(enabled) = self.tools.take() {
            for name in crate::app::builtin_tool_names() {
                self.items.tools.entry(name).or_insert(enabled);
            }
        }
        if let Some(enabled) = self.skills.take() {
            for skill in crate::skills::SkillCatalog::discover(workdir)?.list() {
                self.items.skills.entry(skill.name).or_insert(enabled);
            }
        }
        if let Some(enabled) = self.mcp.take() {
            for server in crate::mcp::configured_server_summaries(workdir)? {
                self.items.mcp.entry(server.name).or_insert(enabled);
            }
        }
        if let Some(enabled) = self.plugins.take() {
            for plugin in crate::plugins::PluginCatalog::discover(workdir)?.summaries()? {
                if let Some(id) = plugin["id"].as_str() {
                    self.items
                        .plugins
                        .entry(id.to_owned())
                        .or_insert(enabled && plugin["requested"].as_bool().unwrap_or(false));
                }
            }
        }
        Ok(())
    }

    pub fn effective(&self, defaults: Capabilities) -> Capabilities {
        let mut items = defaults.items;
        for group in [
            CapabilityGroup::Tools,
            CapabilityGroup::Mcp,
            CapabilityGroup::Skills,
            CapabilityGroup::Plugins,
        ] {
            items
                .group_mut(group)
                .extend(self.items.group(group).clone());
        }
        Capabilities {
            tools: self.tools.unwrap_or(defaults.tools),
            mcp: self.mcp.unwrap_or(defaults.mcp),
            skills: self.skills.unwrap_or(defaults.skills),
            plugins: self.plugins.unwrap_or(defaults.plugins),
            items,
        }
    }
}

pub fn defaults_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .context("HOME/USERPROFILE unavailable")?;
    Ok(PathBuf::from(home).join(".kroma/capabilities.json"))
}

pub(crate) fn read_or_default<T: serde::de::DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid capability settings: {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error.into()),
    }
}

pub fn load_defaults() -> Result<Capabilities> {
    read_or_default(&defaults_path()?)
}

pub fn save_defaults(settings: Capabilities) -> Result<()> {
    save_defaults_at(&defaults_path()?, settings)
}

fn save_defaults_at(path: &Path, settings: Capabilities) -> Result<()> {
    fs::create_dir_all(path.parent().context("capability settings need a parent")?)?;
    crate::catalog_file::update(path, path, || Ok(settings))
}

pub fn patch_defaults(
    group: CapabilityGroup,
    items: &BTreeMap<String, Option<bool>>,
) -> Result<()> {
    patch_defaults_at(&defaults_path()?, group, items)
}

fn patch_defaults_at(
    path: &Path,
    group: CapabilityGroup,
    items: &BTreeMap<String, Option<bool>>,
) -> Result<()> {
    fs::create_dir_all(path.parent().context("capability settings need a parent")?)?;
    crate::catalog_file::update(path, path, || {
        let mut settings: Capabilities = read_or_default(path)?;
        settings.items.patch(group, items, true)?;
        Ok(settings)
    })
}

/// Inventory never starts extension processes, and retains saved, missing items.
pub fn settings_value(
    workdir: &Path,
    session_id: &str,
    tool_definitions: Vec<serde_json::Value>,
) -> Result<serde_json::Value> {
    use serde_json::{json, Value};
    let defaults = load_defaults()?;
    let mut session =
        crate::sessions::SessionCatalog::new(workdir)?.capability_overrides(session_id)?;
    session.expand_legacy(workdir)?;
    let effective = session.effective(defaults.clone());
    let mut warnings = Vec::new();
    let mut inventory: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut tools = BTreeMap::<String, Value>::new();
    for name in crate::app::builtin_tool_names() {
        tools.insert(
            name.clone(),
            json!({"id":name,"name":name,"source":"builtin"}),
        );
    }
    for definition in tool_definitions {
        if let Some(name) = definition["function"]["name"].as_str() {
            tools.insert(name.to_owned(), json!({"id":name,"name":name,"description":definition["function"]["description"],"source":definition.get("source").cloned().unwrap_or(json!("registered"))}));
        }
    }
    let mut plugin_defaults = BTreeMap::new();
    match crate::plugins::PluginCatalog::discover(workdir) {
        Ok(catalog) => {
            warnings.extend(catalog.warnings.clone());
            for definition in catalog.capability_tool_inventory() {
                if let Some(name) = definition["function"]["name"].as_str() {
                    tools.entry(name.to_owned()).or_insert_with(|| json!({"id":name,"name":name,"description":definition["function"]["description"],"source":definition["source"]}));
                }
            }
            match catalog.summaries() {
                Ok(plugins) => {
                    for plugin in plugins {
                        if let Some(id) = plugin["id"].as_str() {
                            plugin_defaults.insert(
                                id.to_owned(),
                                plugin["requested"].as_bool().unwrap_or(false),
                            );
                            inventory.entry("plugins".into()).or_default().push(json!({"id":id,"name":plugin["name"],"description":plugin["description"],"source":if plugin["project"] == true {"project"} else {"user"}}));
                        }
                    }
                }
                Err(error) => warnings.push(format!("Plugin inventory: {error:#}")),
            }
        }
        Err(error) => warnings.push(format!("Plugin inventory: {error:#}")),
    }
    match crate::skills::SkillCatalog::discover(workdir) {
        Ok(catalog) => {
            let skills = catalog.list();
            if !skills.is_empty() {
                tools
                    .entry("load_skill".into())
                    .or_insert(json!({"id":"load_skill","name":"load_skill","source":"skills"}));
            }
            inventory.insert("skills".into(), skills.into_iter().map(|skill| json!({"id":skill.name,"name":skill.name,"description":skill.description,"source":skill.path})).collect());
        }
        Err(error) => warnings.push(format!("Skill inventory: {error:#}")),
    }
    match crate::mcp::configured_server_summaries(workdir) {
        Ok(servers) => {
            if !servers.is_empty() {
                for name in ["mcp_search", "mcp_call"] {
                    tools
                        .entry(name.into())
                        .or_insert(json!({"id":name,"name":name,"source":"mcp"}));
                }
            }
            inventory.insert("mcp".into(), servers.into_iter().map(|server| json!({"id":server.name,"name":server.name,"description":server.transport,"source":server.source})).collect());
        }
        Err(error) => warnings.push(format!("MCP inventory: {error:#}")),
    }
    inventory.insert("tools".into(), tools.into_values().collect());
    for (key, group) in [
        ("tools", CapabilityGroup::Tools),
        ("mcp", CapabilityGroup::Mcp),
        ("skills", CapabilityGroup::Skills),
        ("plugins", CapabilityGroup::Plugins),
    ] {
        let entries = inventory.entry(key.into()).or_default();
        for id in defaults
            .items
            .group(group)
            .keys()
            .chain(session.items.group(group).keys())
        {
            if !entries.iter().any(|item| item["id"].as_str() == Some(id)) {
                entries.push(json!({"id":id,"name":id,"available":false}));
            }
        }
        for item in entries.iter_mut() {
            let id = item["id"].as_str().unwrap_or_default().to_owned();
            let original = group != CapabilityGroup::Plugins
                || plugin_defaults.get(&id).copied().unwrap_or(false);
            item["default_enabled"] = json!(defaults.enabled(group, &id, original));
            item["session_enabled"] = json!(session.items.group(group).get(&id));
            item["effective_enabled"] = json!(effective.enabled(group, &id, original));
            if item.get("available").is_none() {
                item["available"] = json!(true);
            }
            for field in ["description", "source"] {
                if !item[field].is_string() {
                    item.as_object_mut().unwrap().remove(field);
                }
            }
        }
        entries.sort_by(|left, right| left["id"].as_str().cmp(&right["id"].as_str()));
    }
    Ok(
        json!({"items":inventory,"defaults":defaults,"session":session,"effective":effective,"warnings":warnings}),
    )
}

pub fn effective_for(workdir: &Path, session: &str) -> Result<Capabilities> {
    let defaults = load_defaults()?;
    let mut session =
        crate::sessions::SessionCatalog::new(workdir)?.capability_overrides(session)?;
    session.expand_legacy(workdir)?;
    Ok(session.effective(defaults))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_inherits_and_groups_are_independent() -> Result<()> {
        let overrides: CapabilityOverrides =
            serde_json::from_str(r#"{"tools":false,"skills":false,"plugins":null,"mcp":true}"#)?;
        let effective = overrides.effective(Capabilities {
            plugins: false,
            ..Capabilities::default()
        });
        assert_eq!(
            effective,
            Capabilities {
                tools: false,
                mcp: true,
                skills: false,
                plugins: false,
                ..Default::default()
            }
        );
        assert_eq!(
            CapabilityOverrides::default().effective(Capabilities::default()),
            Capabilities::default()
        );
        assert!(serde_json::from_str::<CapabilityOverrides>(r#"{"secret":"no"}"#).is_err());
        Ok(())
    }

    #[test]
    fn item_patches_are_partial_nullable_only_for_sessions_and_override_legacy_false() -> Result<()>
    {
        let mut defaults: Capabilities =
            serde_json::from_str(r#"{"tools":false,"skills":false,"plugins":false,"mcp":false}"#)?;
        defaults.items.patch(
            CapabilityGroup::Tools,
            &BTreeMap::from([
                ("shell".into(), Some(true)),
                ("history_read".into(), Some(false)),
            ]),
            true,
        )?;
        defaults.items.patch(
            CapabilityGroup::Skills,
            &BTreeMap::from([("one".into(), Some(true)), ("two".into(), Some(false))]),
            true,
        )?;
        let before = defaults.clone();
        assert!(defaults
            .items
            .patch(
                CapabilityGroup::Tools,
                &BTreeMap::from([("shell".into(), None), ("edit_file".into(), Some(true))]),
                true
            )
            .is_err());
        assert_eq!(defaults, before);
        let mut session = CapabilityOverrides::default();
        session.items.patch(
            CapabilityGroup::Tools,
            &BTreeMap::from([
                ("shell".into(), Some(false)),
                ("history_read".into(), Some(true)),
            ]),
            false,
        )?;
        session.items.patch(
            CapabilityGroup::Plugins,
            &BTreeMap::from([("plugin".into(), Some(true))]),
            false,
        )?;
        let effective = session.effective(defaults.clone());
        assert!(!effective.enabled(CapabilityGroup::Tools, "shell", true));
        assert!(effective.enabled(CapabilityGroup::Tools, "history_read", true));
        assert!(effective.enabled(CapabilityGroup::Plugins, "plugin", false));
        assert!(effective.enabled(CapabilityGroup::Skills, "one", true));
        assert!(!effective.enabled(CapabilityGroup::Skills, "two", true));
        session.items.patch(
            CapabilityGroup::Tools,
            &BTreeMap::from([("shell".into(), None)]),
            false,
        )?;
        assert!(session
            .effective(defaults.clone())
            .enabled(CapabilityGroup::Tools, "shell", true));
        assert_eq!(defaults.items.skills, before.items.skills);
        let json = serde_json::to_vec(&session)?;
        assert_eq!(
            serde_json::from_slice::<CapabilityOverrides>(&json)?,
            session
        );
        Ok(())
    }

    #[test]
    fn defaults_roundtrip_and_missing_file_is_all_enabled() -> Result<()> {
        let dir = std::env::temp_dir().join(format!(
            "kroma-capabilities-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        let path = dir.join("capabilities.json");
        assert_eq!(
            read_or_default::<Capabilities>(&path)?,
            Capabilities::default()
        );
        let settings = Capabilities {
            plugins: false,
            mcp: false,
            ..Capabilities::default()
        };
        save_defaults_at(&path, settings.clone())?;
        assert_eq!(read_or_default::<Capabilities>(&path)?, settings);
        patch_defaults_at(
            &path,
            CapabilityGroup::Tools,
            &BTreeMap::from([
                ("shell".into(), Some(false)),
                ("edit_file".into(), Some(true)),
            ]),
        )?;
        patch_defaults_at(
            &path,
            CapabilityGroup::Skills,
            &BTreeMap::from([("skill".into(), Some(false))]),
        )?;
        patch_defaults_at(
            &path,
            CapabilityGroup::Tools,
            &BTreeMap::from([("shell".into(), Some(true))]),
        )?;
        let patched: Capabilities = read_or_default(&path)?;
        assert_eq!(patched.items.tools.get("shell"), Some(&true));
        assert_eq!(patched.items.tools.get("edit_file"), Some(&true));
        assert_eq!(patched.items.skills.get("skill"), Some(&false));
        assert_eq!(patched.plugins, settings.plugins);
        assert_eq!(patched.mcp, settings.mcp);
        let before = fs::read(&path)?;
        assert!(patch_defaults_at(
            &path,
            CapabilityGroup::Tools,
            &BTreeMap::from([("shell".into(), None)])
        )
        .is_err());
        assert_eq!(fs::read(&path)?, before);
        fs::remove_dir_all(dir)?;
        Ok(())
    }
}
