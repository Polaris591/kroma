//! Agent Skills discovery and progressive disclosure. Only metadata goes
//! into the system projection; SKILL.md is read when the model calls a tool.
//! Scripts and reference files remain accessible through the normal shell.
use crate::{
    protocol::ToolCall,
    tools::{Tool, ToolContext, ToolFuture, ToolResult},
};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

#[derive(Clone, Default)]
pub struct SkillCatalog {
    skills: Arc<BTreeMap<String, Skill>>,
}

#[derive(Deserialize)]
struct Frontmatter {
    name: String,
    description: String,
}

fn parse_skill(path: &Path) -> Result<Skill> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("cannot read skill {}", path.display()))?;
    let header = contents
        .strip_prefix("---\n")
        .or_else(|| contents.strip_prefix("---\r\n"))
        .context("SKILL.md must begin with YAML frontmatter")?;
    let boundary = header
        .find("\n---\n")
        .or_else(|| header.find("\n---\r\n"))
        .context("SKILL.md is missing its closing YAML delimiter")?;
    let meta: Frontmatter = serde_yaml::from_str(&header[..boundary])
        .with_context(|| format!("invalid YAML frontmatter in {}", path.display()))?;
    let expected = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|s| s.to_str())
        .context("skill path must have a directory name")?;
    let valid_name = !meta.name.is_empty()
        && meta.name.len() <= 64
        && meta.name == expected
        && !meta.name.starts_with('-')
        && !meta.name.ends_with('-')
        && !meta.name.contains("--")
        && meta
            .name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !valid_name {
        bail!(
            "invalid skill name '{}' (must match its directory)",
            meta.name
        );
    }
    let description = meta.description.trim();
    if description.is_empty() || description.len() > 1024 {
        bail!(
            "skill '{}' description must contain 1..1024 characters",
            meta.name
        );
    }
    Ok(Skill {
        name: meta.name,
        description: description.to_owned(),
        path: path.to_owned(),
    })
}

impl SkillCatalog {
    pub fn discover(workdir: &Path) -> Result<Self> {
        let home = env::var_os("HOME").or_else(|| env::var_os("USERPROFILE"));
        let mut roots = Vec::new();
        if let Some(home) = home {
            let home = PathBuf::from(home);
            roots.push(home.join(".agents/skills"));
            roots.push(home.join(".kroma/skills"));
        }
        roots.push(workdir.join(".agents/skills"));
        roots.push(workdir.join(".kroma/skills"));
        Self::from_roots(&roots)
    }

    pub fn from_roots(roots: &[PathBuf]) -> Result<Self> {
        let mut skills = BTreeMap::new();
        // Later roots override earlier roots, so project skills override
        // global ones, and the Kroma-specific directory wins over .agents.
        for root in roots {
            let read_dir = match fs::read_dir(root) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error).with_context(|| format!("reading {}", root.display()))
                }
            };
            let mut entries = read_dir.collect::<std::io::Result<Vec<_>>>()?;
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let path = entry.path().join("SKILL.md");
                if !path.is_file() {
                    continue;
                }
                match parse_skill(&path) {
                    Ok(skill) => {
                        skills.insert(skill.name.clone(), skill);
                    }
                    Err(error) => {
                        crate::runtime_log!("[skills] skipping {}: {error:#}", path.display())
                    }
                }
            }
        }
        Ok(Self {
            skills: Arc::new(skills),
        })
    }

    pub fn list(&self) -> Vec<Skill> {
        self.skills.values().cloned().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    pub fn metadata_prompt(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut text = String::from(
            "\n\nAvailable Agent Skills (metadata only). When a task matches a skill's description, call load_skill with its name before following its instructions. Load referenced files using shell only when necessary. Do not read every skill by default:\n"
        );
        for skill in self.skills.values() {
            text.push_str(&format!("- {}: {}\n", skill.name, skill.description));
        }
        Some(text)
    }

    pub fn load(&self, name: &str) -> Result<Value> {
        let skill = self
            .skills
            .get(name)
            .with_context(|| format!("unknown skill '{name}'"))?;
        // Reload from disk so users can edit skills without restarting the
        // process. Do not accidentally load arbitrary paths from model input.
        let latest = parse_skill(&skill.path)?;
        if latest.name != skill.name {
            bail!("skill '{}' has changed identity on disk", skill.name);
        }
        let contents = fs::read_to_string(&skill.path)?;
        Ok(json!({
            "name": skill.name,
            "skill_directory": skill.path.parent().unwrap().display().to_string(),
            "instructions": contents,
            "resource_hint": "Use shell to read referenced files relative to skill_directory, or to run the skill's scripts when requested."
        }))
    }
}

pub struct LoadSkill(pub SkillCatalog);

impl Tool for LoadSkill {
    fn name(&self) -> &str {
        "load_skill"
    }

    fn definition(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "load_skill",
                "description": "Load the full SKILL.md instructions of a relevant Agent Skill by name. Use only when its metadata matches the current task.",
                "parameters": {
                    "type": "object",
                    "properties": {"name":{"type":"string","description":"Exact available skill name"}},
                    "required": ["name"], "additionalProperties":false
                }
            }
        })
    }

    fn execute<'a>(&'a self, call: &'a ToolCall, _: ToolContext<'a>) -> ToolFuture<'a> {
        Box::pin(async move {
            let result = (|| -> Result<Value> {
                let args: Value = serde_json::from_str(&call.function.arguments)?;
                let name = args["name"].as_str().context("missing name")?;
                self.0.load(name)
            })();
            match result {
                Ok(result) => ToolResult::ok(result.to_string()),
                Err(error) => ToolResult::error(json!({"error":error.to_string()}).to_string()),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn discovers_valid_skills_and_project_override_without_eager_body() -> Result<()> {
        let root = env::temp_dir().join(format!(
            "kroma-skills-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        let global = root.join("global");
        let project = root.join("project");
        let invalid = root.join("invalid");
        fs::create_dir_all(global.join("code-review"))?;
        fs::create_dir_all(project.join("code-review"))?;
        fs::create_dir_all(project.join("data-analysis"))?;
        fs::create_dir_all(invalid.join("BAD"))?;
        fs::write(
            global.join("code-review/SKILL.md"),
            "---\nname: code-review\ndescription: Global review\n---\nGlobal body SECRET_GLOBAL",
        )?;
        fs::write(project.join("code-review/SKILL.md"),
            "---\nname: code-review\ndescription: >-\n  Review Rust code\n  for maintainability\n---\nLocal body SECRET_LOCAL")?;
        fs::write(
            project.join("data-analysis/SKILL.md"),
            "---\nname: data-analysis\ndescription: Analyze data\n---\nAnalyze it",
        )?;
        fs::write(
            invalid.join("BAD/SKILL.md"),
            "---\nname: BAD\ndescription: Invalid\n---\nBody",
        )?;
        let catalog = SkillCatalog::from_roots(&[global, project, invalid])?;
        assert_eq!(catalog.list().len(), 2);
        let prompt = catalog.metadata_prompt().unwrap();
        assert!(prompt.contains("Review Rust code for maintainability"));
        assert!(!prompt.contains("SECRET_LOCAL"));
        assert!(!prompt.contains("SECRET_GLOBAL"));
        let loaded = catalog.load("code-review")?;
        assert!(loaded["instructions"]
            .as_str()
            .unwrap()
            .contains("SECRET_LOCAL"));
        assert!(!loaded["instructions"]
            .as_str()
            .unwrap()
            .contains("SECRET_GLOBAL"));
        assert!(catalog.load("../code-review").is_err());
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn reloading_the_body_does_not_require_a_new_discovery() -> Result<()> {
        let root = env::temp_dir().join(format!(
            "kroma-skill-refresh-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        let dir = root.join("skills/code-review");
        fs::create_dir_all(&dir)?;
        let path = dir.join("SKILL.md");
        fs::write(
            &path,
            "---\nname: code-review\ndescription: Review code\n---\nFirst",
        )?;
        let catalog = SkillCatalog::from_roots(&[root.join("skills")])?;
        fs::write(
            &path,
            "---\nname: code-review\ndescription: Review code\n---\nUpdated",
        )?;
        assert!(catalog.load("code-review")?["instructions"]
            .as_str()
            .unwrap()
            .contains("Updated"));
        fs::remove_dir_all(root)?;
        Ok(())
    }
}
