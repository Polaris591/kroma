//! Deterministic projection-only prompts: never mutate the durable transcript.
use crate::protocol::Message;
use anyhow::{bail, Context, Result};
use std::{env, fs, io, path::Path};

pub const EXECUTION_GUIDANCE: &str = "\nFor coding tasks, focus inspections on relevant files; once the needed evidence is found, edit rather than re-inspecting unchanged files. When necessary inspections are independent, request them together as separate shell calls (or one concise command); keep dependent edits and checks in order. After implementing the requested changes, run the requested tests and relevant regression checks. If all requirements are met and those checks pass, finish with a concise result rather than running unrelated doctests, broad exploratory checks or repeated successful tests. If a check fails, investigate its actual cause and re-run after a meaningful fix. Do not omit required verification just to save calls.";

/// A simple ordered list of projection-only prompt contributions. No event
/// hooks, mutable global state, dynamic templates or plugin runtime.
pub struct PromptComposer {
    system_override: Option<String>,
    /// Embedding hosts may intentionally persist a custom system identity;
    /// plugin replacements remain request-only and never set this field.
    initial_system_override: Option<String>,
    additions: Vec<String>,
    instruction_files: Vec<String>,
    trailing: Vec<String>,
    custom_tools: bool,
}

impl Default for PromptComposer {
    fn default() -> Self {
        Self {
            system_override: None,
            initial_system_override: None,
            additions: vec![EXECUTION_GUIDANCE.to_owned()],
            instruction_files: Vec::new(),
            trailing: Vec::new(),
            custom_tools: false,
        }
    }
}

impl PromptComposer {
    /// Reload once per task. Files affect only model request projections:
    /// session snapshots and WAL retain the original stable system message.
    pub fn refresh_instruction_files(&mut self, workdir: &Path) -> Result<()> {
        self.refresh_for_task(workdir, "")
    }

    /// Load nearby AGENTS.md only for explicitly referenced project paths.
    /// Scanning every nested AGENTS.md on every turn is expensive and can
    /// mix instructions for unrelated components.
    pub fn refresh_for_task(&mut self, workdir: &Path, task: &str) -> Result<()> {
        let global = env::var_os("HOME")
            .or_else(|| env::var_os("USERPROFILE"))
            .map(|home| std::path::PathBuf::from(home).join(".kroma/AGENTS.md"));
        let project = workdir.join("AGENTS.md");
        self.load_instruction_paths(global.as_deref(), &project)?;
        let root = workdir
            .canonicalize()
            .unwrap_or_else(|_| workdir.to_path_buf());
        let mut included = std::collections::BTreeSet::new();
        for token in task.split_whitespace() {
            let explicit = token.starts_with('@');
            let token = token
                .trim_start_matches('@')
                .trim_matches(|c: char| matches!(c, ',' | ';' | ')' | '(' | '\'' | '"'));
            // Don't stat/canonicalize every prose word in a long prompt.
            // An ordinary "hello" is not an explicit reference to ./hello
            // even if a directory of that name happens to exist.
            if !explicit && !token.contains(['/', '\\']) && Path::new(token).extension().is_none() {
                continue;
            }
            let candidate = if Path::new(token).is_absolute() {
                std::path::PathBuf::from(token)
            } else {
                workdir.join(token)
            };
            let Ok(path) = candidate.canonicalize() else {
                continue;
            };
            // Canonicalize the root too: on Windows `canonicalize` returns a
            // `\\?\C:\...` verbatim path, so comparing it with a plain workdir
            // never matched and nested instructions were silently dropped.
            if !path.starts_with(&root) {
                continue;
            }
            let mut dir = if path.is_dir() {
                path
            } else {
                path.parent().unwrap_or(&root).to_path_buf()
            };
            let mut chain = Vec::new();
            while dir.starts_with(&root) && dir != root {
                chain.push(dir.clone());
                dir = dir.parent().unwrap_or(&root).to_path_buf();
            }
            for folder in chain.into_iter().rev() {
                let nested = folder.join("AGENTS.md");
                if included.insert(nested.clone()) {
                    if let Some(contents) = read_instruction_file(&nested)? {
                        self.instruction_files.push(format!(
                            "\n\nNested instructions ({}):\n{contents}",
                            nested.display()
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn load_instruction_paths(&mut self, global: Option<&Path>, project: &Path) -> Result<()> {
        let mut loaded = Vec::new();
        if let Some(global) = global {
            if let Some(text) = read_instruction_file(global)? {
                loaded.push(format!(
                    "\n\nGlobal instructions (~/.kroma/AGENTS.md):\n{text}"
                ));
            }
        }
        if let Some(text) = read_instruction_file(project)? {
            loaded.push(format!("\n\nProject instructions (AGENTS.md):\n{text}"));
        }
        self.instruction_files = loaded;
        Ok(())
    }

    pub(crate) fn for_registered_tools(mut self, builtin_only: bool) -> Self {
        self.custom_tools = !builtin_only;
        self
    }

    /// Replace the base system instruction when an embedding application
    /// explicitly chooses to do so. The default text remains byte-identical.
    pub fn with_system_prompt(mut self, system: impl Into<String>) -> Result<Self> {
        let system = system.into();
        self.replace_system_prompt(system.clone())?;
        self.initial_system_override = Some(system);
        Ok(self)
    }

    pub fn replace_system_prompt(&mut self, system: impl Into<String>) -> Result<()> {
        let system = system.into();
        if system.trim().is_empty() {
            bail!("base system prompt must not be empty");
        }
        self.system_override = Some(system);
        Ok(())
    }

    pub fn with_extra(mut self, segment: impl Into<String>) -> Result<Self> {
        let segment = segment.into();
        if segment.trim().is_empty() {
            bail!("prompt contribution must not be empty");
        }
        if !self.additions.contains(&segment) {
            self.additions.push(segment);
        }
        Ok(self)
    }

    /// Host/extension contributions after global and project instructions.
    /// Skill metadata uses this instead of coupling Agent to SkillCatalog.
    pub fn with_trailing(mut self, segment: impl Into<String>) -> Result<Self> {
        self.push_trailing(segment)?;
        Ok(self)
    }

    pub fn push_trailing(&mut self, segment: impl Into<String>) -> Result<()> {
        let segment = segment.into();
        if segment.trim().is_empty() {
            bail!("prompt contribution must not be empty");
        }
        if !self.trailing.contains(&segment) {
            self.trailing.push(segment);
        }
        Ok(())
    }

    pub fn initial_history(&self, workdir: &Path) -> Vec<Message> {
        // Host-created agents retain their explicitly chosen system identity;
        // plugin replacements are intentionally absent from persisted history.
        if let Some(system) = &self.initial_system_override {
            return vec![Message::new("system", system)];
        }
        let tool_description = if self.custom_tools {
            "Your available tools are provided in the request tool definitions."
        } else {
            "Your only tool is shell. Use it to inspect and work with files when needed."
        };
        vec![Message::new(
            "system",
            format!(
                "You are Kroma, an AI coding agent. {tool_description} The starting working directory is {}. Be accurate about command results and do not claim a command ran unless a tool result confirms it. Avoid destructive commands unless explicitly requested. Reply concisely in the user's language.",
                workdir.display()
            ),
        )]
    }

    pub fn apply(&self, view: &mut [Message]) {
        if let Some(system) = view.first_mut().and_then(|msg| msg.content.as_mut()) {
            if let Some(replacement) = &self.system_override {
                *system = replacement.clone();
            } else if self.custom_tools {
                // Old sessions can enable new tools without persisting a
                // misleading claim that only Shell is available.
                *system = system.replace(
                    "Your only tool is shell. Use it to inspect and work with files when needed.",
                    "Your available tools are provided in the request tool definitions.",
                );
            } else {
                // Reopening a session created with a custom registry and the
                // stock prompt must not keep advertising disabled tools.
                *system = system.replace(
                    "Your available tools are provided in the request tool definitions.",
                    "Your only tool is shell. Use it to inspect and work with files when needed.",
                );
            }
            // Append the ordered contribution block as a whole. A project
            // prompt quoting an extension's text must not suppress that
            // extension; `contains()` confused mentions with activation.
            // Request projections start from the persisted system message.
            let additions = self
                .additions
                .iter()
                .chain(&self.instruction_files)
                .chain(&self.trailing)
                .map(String::as_str)
                .collect::<String>();
            if !additions.is_empty() && !system.ends_with(&additions) {
                system.push_str(&additions);
            }
        }
    }

    /// Extra projection budget relative to the *old* built-in guidance,
    /// which was already omitted from the legacy TRC/APEC estimate.
    /// Only extensions or changed base prompts affect the planning heuristic.
    pub fn extra_estimated_tokens(&self, stored_system: &Message) -> usize {
        let Some(content) = stored_system.content.as_deref() else {
            return 0;
        };
        let mut active = vec![Message::new("system", content)];
        let mut baseline = active.clone();
        PromptComposer::default().apply(&mut baseline);
        self.apply(&mut active);
        let actual_bytes = active[0].content.as_ref().map_or(0, String::len);
        let baseline_bytes = baseline[0].content.as_ref().map_or(0, String::len);
        actual_bytes.saturating_sub(baseline_bytes).div_ceil(3)
    }
}

fn read_instruction_file(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok((!contents.trim().is_empty()).then_some(contents)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("cannot read instructions at {}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn global_and_project_instructions_are_projected_in_order_and_reload() -> Result<()> {
        let root = env::temp_dir().join(format!(
            "kroma-instructions-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
        ));
        let global = root.join("home/.kroma/AGENTS.md");
        let project = root.join("repo/AGENTS.md");
        fs::create_dir_all(global.parent().unwrap())?;
        fs::create_dir_all(project.parent().unwrap())?;
        fs::write(&global, "GLOBAL_MARKER")?;
        fs::write(&project, "PROJECT_MARKER")?;
        let mut composer = PromptComposer::default();
        composer.load_instruction_paths(Some(&global), &project)?;
        let original = composer.initial_history(project.parent().unwrap());
        let mut outbound = original.clone();
        composer.apply(&mut outbound);
        let text = outbound[0].content.clone().unwrap();
        assert!(text.find("GLOBAL_MARKER").unwrap() < text.find("PROJECT_MARKER").unwrap());
        assert!(text.find("PROJECT_MARKER").unwrap() > text.find(EXECUTION_GUIDANCE).unwrap());
        assert!(!original[0]
            .content
            .as_ref()
            .unwrap()
            .contains("GLOBAL_MARKER"));
        assert!(composer.extra_estimated_tokens(&original[0]) > 0);
        // Multiple projections never accumulate duplicate content.
        composer.apply(&mut outbound);
        assert_eq!(outbound[0].content.as_deref(), Some(text.as_str()));
        fs::remove_file(&global)?;
        fs::write(&project, "UPDATED_PROJECT_MARKER")?;
        composer.load_instruction_paths(Some(&global), &project)?;
        let mut updated = original;
        composer.apply(&mut updated);
        let updated = updated[0].content.as_deref().unwrap();
        assert!(!updated.contains("GLOBAL_MARKER"));
        assert!(updated.contains("UPDATED_PROJECT_MARKER"));
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn quoted_extension_text_does_not_suppress_its_contribution() -> Result<()> {
        let snippet = "MANDATORY_EXTRA_DIRECTIVE";
        let composer = PromptComposer::default().with_extra(snippet)?;
        let mut view = vec![Message::new(
            "system",
            format!(
                "Example from repository documentation: '{snippet}' (not an active instruction)."
            ),
        )];
        composer.apply(&mut view);
        let first = view[0].content.clone().unwrap();
        assert!(
            first.ends_with(snippet),
            "extension is an active final contribution"
        );
        assert_eq!(
            first.matches(snippet).count(),
            2,
            "quoted mention and contribution both remain"
        );
        composer.apply(&mut view);
        assert_eq!(
            view[0].content.as_deref(),
            Some(first.as_str()),
            "re-applying is idempotent"
        );
        Ok(())
    }

    #[test]
    fn absent_files_preserve_the_existing_system_prompt() -> Result<()> {
        let root = env::temp_dir().join(format!(
            "kroma-missing-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        let mut composer = PromptComposer::default();
        composer.load_instruction_paths(Some(&root.join("missing")), &root.join("project"))?;
        let raw = composer.initial_history(Path::new("/tmp/mock"));
        let mut projection = raw.clone();
        composer.apply(&mut projection);
        let mut baseline = raw;
        PromptComposer::default().apply(&mut baseline);
        assert_eq!(projection[0].content, baseline[0].content);
        Ok(())
    }

    #[test]
    fn prose_words_do_not_probe_or_load_matching_nested_folders() -> Result<()> {
        let root = env::temp_dir().join(format!("kroma-prose-paths-{}", std::process::id()));
        fs::create_dir_all(root.join("hello"))?;
        fs::write(root.join("hello/AGENTS.md"), "SHOULD_NOT_BE_INJECTED")?;
        let mut composer = PromptComposer::default();
        composer.refresh_for_task(&root, &"hello ".repeat(4_000))?;
        let mut view = composer.initial_history(&root);
        composer.apply(&mut view);
        assert!(!view[0]
            .content
            .as_deref()
            .unwrap()
            .contains("SHOULD_NOT_BE_INJECTED"));
        composer.refresh_for_task(&root, "Inspect @hello")?;
        let mut view = composer.initial_history(&root);
        composer.apply(&mut view);
        assert!(view[0]
            .content
            .as_deref()
            .unwrap()
            .contains("SHOULD_NOT_BE_INJECTED"));
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn nested_instructions_apply_only_to_explicitly_referenced_files() -> Result<()> {
        let root = env::temp_dir().join(format!("kroma-prompt-nested-{}", std::process::id()));
        fs::create_dir_all(root.join("src/nested"))?;
        fs::create_dir_all(root.join("other"))?;
        fs::write(root.join("AGENTS.md"), "ROOT_INSTRUCTION")?;
        fs::write(root.join("src/AGENTS.md"), "SRC_INSTRUCTION")?;
        fs::write(root.join("src/nested/AGENTS.md"), "NESTED_INSTRUCTION")?;
        fs::write(root.join("other/AGENTS.md"), "UNRELATED_INSTRUCTION")?;
        fs::write(root.join("src/nested/file.rs"), "code")?;
        let mut composer = PromptComposer::default();
        composer.refresh_for_task(&root, "Analyze @src/nested/file.rs")?;
        let mut view = composer.initial_history(&root);
        composer.apply(&mut view);
        let prompt = view[0].content.as_ref().unwrap();
        assert!(prompt.contains("ROOT_INSTRUCTION"));
        assert!(prompt.contains("SRC_INSTRUCTION"));
        assert!(prompt.contains("NESTED_INSTRUCTION"));
        assert!(!prompt.contains("UNRELATED_INSTRUCTION"));
        fs::remove_dir_all(root)?;
        Ok(())
    }
}
