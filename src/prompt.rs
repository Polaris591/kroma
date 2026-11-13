//! Deterministic projection-only prompts: never mutate the durable transcript.
use crate::protocol::Message;
use anyhow::{bail, Context, Result};
use std::{
    collections::BTreeSet,
    env, fs, io,
    path::{Path, PathBuf},
};

// Single production instruction. The concatenated bytes intentionally match
// the previously tested evidence-based completion + quality guidance.
pub const EXECUTION_GUIDANCE: &str = concat!(
    "\nFor coding tasks, locate relevant symbols before reading large files and keep inspections focused on relevant files and ranges; prefer focused ranges and diagnostic excerpts over dumping entire files or logs unless full context is necessary; once the needed evidence is found, edit rather than re-inspecting unchanged files. When necessary inspections are independent, request them together as separate shell calls (or one concise command); keep dependent edits and checks in order. ",
    "After repairing the failure, run the requested tests and relevant checks; if the requirements are met and those checks pass, finish concisely. Do not intentionally revert a passing fix just to recreate the old failure unless the user asks for a negative control or unresolved evidence makes it necessary. If a passing test leaves a concrete requirement unverified, check that requirement before finishing.",
    " If a check fails, investigate its actual cause and re-run after a meaningful fix. Do not omit required verification just to save calls.",
    "\nGive a brief user-facing progress update when starting substantial work, making an important confirmed discovery or changing direction, and starting validation. Describe actions or verified results, not internal reasoning. Skip routine tool-by-tool narration and progress updates for simple questions; do not add model requests or split batchable operations just to report progress. When you finish, reply concisely and accurately about what changed and what was verified. Complex tasks may warrant a fuller explanation of the modifications, verification evidence, failure causes and remaining risks; report uncertainty rather than hiding it.",
    "\nPrefer the simplest correct implementation consistent with the existing codebase. Before adding code, prefer suitable project implementations, then standard-library or platform capabilities and existing dependencies, before writing new code. Avoid redundant comments, explanatory boilerplate, unnecessary helper layers, and unrelated refactoring. Preserve required APIs, edge cases, tests, and non-obvious invariants. Never sacrifice correctness just to reduce code length.",
    "\nInvestigate tooling internals only when the evidence points there."
);

/// A simple ordered list of projection-only prompt contributions. No event
/// hooks, mutable global state, dynamic templates or plugin runtime.
#[derive(Clone)]
pub struct PromptComposer {
    system_override: Option<String>,
    /// Embedding hosts may intentionally persist a custom system identity;
    /// plugin replacements remain request-only and never set this field.
    initial_system_override: Option<String>,
    additions: Vec<String>,
    instruction_files: Vec<String>,
    nested_paths: BTreeSet<PathBuf>,
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
            nested_paths: BTreeSet::new(),
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
        self.include_discovered_paths(workdir, task)?;
        Ok(())
    }

    /// Tool outputs and arguments can reveal a nested component that the
    /// original user request did not name. Load only instructions on paths
    /// actually discovered in this workspace, never scan unrelated trees.
    /// Returns whether the model-facing instruction projection changed.
    pub(crate) fn include_discovered_paths(&mut self, workdir: &Path, text: &str) -> Result<bool> {
        let root = workdir
            .canonicalize()
            .unwrap_or_else(|_| workdir.to_path_buf());
        let mut changed = false;
        // Tool output is unbounded; a short bounded inspection keeps ordinary
        // logs from turning into a filesystem traversal.
        let mut previous = "";
        for token in text.split_whitespace().take(4096) {
            let explicit = token.starts_with('@');
            let token = token.trim_start_matches('@').trim_matches(|c: char| {
                matches!(
                    c,
                    ',' | ';' | ')' | '(' | '[' | ']' | '\'' | '"' | '`' | '{' | '}' | ':' | '>'
                )
            });
            // Strip standard source-location suffixes (file.rs:12 or file.rs:12:4)
            // without treating the colon in a Windows drive letter as a suffix.
            let mut token = token;
            for _ in 0..2 {
                if let Some((file, line)) = token.rsplit_once(':') {
                    if !file.is_empty() && line.parse::<usize>().is_ok() {
                        token = file;
                    }
                }
            }
            // Don't stat/canonicalize every prose word in a long prompt.
            // An ordinary "hello" is not an explicit reference to ./hello
            // even if a directory of that name happens to exist.
            let directory_argument = matches!(previous, "cd" | "pushd" | "--workdir");
            previous = token;
            if !explicit
                && !directory_argument
                && !token.contains(['/', '\\'])
                && Path::new(token).extension().is_none()
            {
                continue;
            }
            let candidate = if Path::new(token).is_absolute() {
                std::path::PathBuf::from(token)
            } else {
                workdir.join(token)
            };
            // A new file does not exist yet. Resolve its nearest existing
            // ancestor so a first-write into an existing nested directory
            // still loads that directory's AGENTS.md before execution.
            let mut existing = candidate.as_path();
            while !existing.exists() {
                let Some(parent) = existing.parent() else {
                    break;
                };
                if parent == existing {
                    break;
                }
                existing = parent;
            }
            let Ok(path) = existing.canonicalize() else {
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
                if !self.nested_paths.contains(&nested) {
                    if let Some(contents) = read_instruction_file(&nested)? {
                        self.nested_paths.insert(nested.clone());
                        self.instruction_files.push(format!(
                            "\n\nNested instructions ({}):\n{contents}",
                            crate::path_display::for_shell(&nested)
                        ));
                        changed = true;
                    }
                }
            }
        }
        Ok(changed)
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
        self.nested_paths.clear();
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

    /// Embedding hosts may replace the default guidance in their own runtime.
    /// This is a generic API, not a production experiment switch.
    pub fn replace_execution_guidance(&mut self, guidance: impl Into<String>) -> Result<()> {
        let guidance = guidance.into();
        if guidance.trim().is_empty() {
            bail!("replacement execution guidance must not be empty");
        }
        let original = self
            .additions
            .iter_mut()
            .find(|value| value.as_str() == EXECUTION_GUIDANCE)
            .context("default guidance is not present")?;
        *original = guidance;
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
                crate::path_display::for_shell(workdir)
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

    /// Estimated tokens of each request-only prompt segment, measured with the
    /// same byte heuristic the budget uses. Reported separately so the Desktop
    /// can show where the system prompt budget actually goes.
    pub fn segment_tokens(&self) -> (usize, usize, usize) {
        let tokens = |text: &String| crate::token_estimate::content_tokens(text.as_bytes());
        (
            self.additions.iter().map(tokens).sum(),
            self.instruction_files.iter().map(tokens).sum(),
            self.trailing.iter().map(tokens).sum(),
        )
    }

    /// Token estimate for the complete system prompt relative to the
    /// persisted system message. The persisted message is the raw baseline;
    /// comparing against a default-composed baseline would hide the built-in
    /// guidance that is appended only in the request projection.
    pub fn extra_estimated_tokens(&self, stored_system: &Message) -> usize {
        let Some(content) = stored_system.content.as_deref() else {
            return 0;
        };
        let stored_bytes = content.len();
        let mut active = vec![Message::new("system", content)];
        self.apply(&mut active);
        let actual_bytes = active[0].content.as_ref().map_or(0, String::len);
        actual_bytes.saturating_sub(stored_bytes).div_ceil(3)
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
    fn plugin_extra_guidance_preserves_the_single_default_instruction() {
        let composer = PromptComposer::default()
            .with_extra("\nPlugin-specific extra instruction.")
            .unwrap();
        let mut view = composer.initial_history(Path::new("."));
        composer.apply(&mut view);
        let system = view[0].content.as_deref().unwrap();
        assert!(system.contains("Plugin-specific extra instruction."));
        assert_eq!(system.matches(EXECUTION_GUIDANCE).count(), 1);
        assert!(composer.with_extra("").is_err());
    }

    #[test]
    fn same_length_instruction_changes_are_visible_and_budgeted() -> Result<()> {
        let root = env::temp_dir().join(format!(
            "kroma-same-length-instructions-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
        ));
        fs::create_dir_all(&root)?;
        let project = root.join("AGENTS.md");
        let missing = root.join("missing/AGENTS.md");
        fs::write(&project, "ALPHA")?;
        let mut composer = PromptComposer::default();
        composer.load_instruction_paths(Some(&missing), &project)?;
        let raw = composer.initial_history(&root);
        let mut first = raw.clone();
        composer.apply(&mut first);
        let first_text = first[0].content.clone().unwrap();
        let first_extra = composer.extra_estimated_tokens(&raw[0]);

        fs::write(&project, "BRAVO")?;
        composer.load_instruction_paths(Some(&missing), &project)?;
        let mut second = raw.clone();
        composer.apply(&mut second);
        let second_text = second[0].content.clone().unwrap();
        assert_ne!(first_text, second_text);
        assert_eq!(first_text.len(), second_text.len());
        assert_eq!(first_extra, composer.extra_estimated_tokens(&raw[0]));

        fs::write(&project, "BRAVO_LONGER")?;
        composer.load_instruction_paths(Some(&missing), &project)?;
        let mut longer = raw.clone();
        composer.apply(&mut longer);
        assert!(composer.extra_estimated_tokens(&raw[0]) > first_extra);
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn one_default_guidance_contains_completion_and_quality_rules() {
        for rule in [
            "Do not intentionally revert a passing fix",
            "If a passing test leaves a concrete requirement unverified",
            "Prefer the simplest correct implementation",
            "Never sacrifice correctness",
            "Give a brief user-facing progress update",
        ] {
            assert!(EXECUTION_GUIDANCE.contains(rule), "{rule}");
        }
        assert!(!EXECUTION_GUIDANCE.contains("KROMA_EXPERIMENT_"));
    }

    #[test]
    fn production_guidance_is_projected_once_without_mutating_history() {
        let composer = PromptComposer::default();
        let original = composer.initial_history(Path::new("/tmp/kroma-guidance-test"));
        let stored = original[0].content.clone().unwrap();
        assert!(!stored.contains(EXECUTION_GUIDANCE));
        let mut view = original.clone();
        composer.apply(&mut view);
        let projected = view[0].content.clone().unwrap();
        assert!(projected.ends_with(EXECUTION_GUIDANCE));
        assert_eq!(projected.matches(EXECUTION_GUIDANCE).count(), 1);
        composer.apply(&mut view);
        assert_eq!(view[0].content.as_deref(), Some(projected.as_str()));
        assert_eq!(original[0].content.as_deref(), Some(stored.as_str()));
        assert_eq!(
            composer.extra_estimated_tokens(&original[0]),
            (projected.len() - stored.len()).div_ceil(3)
        );
    }

    #[test]
    fn built_in_guidance_does_not_repeat_execution_directives() {
        for directive in [
            "If a check fails",
            "Do not intentionally revert a passing fix",
            "locate relevant symbols",
            "requested tests",
            "re-inspecting unchanged files",
        ] {
            assert_eq!(EXECUTION_GUIDANCE.matches(directive).count(), 1);
        }
    }

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
        composer.refresh_for_task(&root, "See `src/nested/file.rs:13:2` for the error")?;
        let mut view = composer.initial_history(&root);
        composer.apply(&mut view);
        assert!(
            view[0]
                .content
                .as_deref()
                .unwrap()
                .contains("NESTED_INSTRUCTION"),
            "backtick-and-line-number references must load nested instructions"
        );
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn tool_discovered_nested_instructions_extend_next_projection_once() -> Result<()> {
        let root = env::temp_dir().join(format!(
            "kroma-discovered-instructions-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir_all(root.join("src/deep"))?;
        fs::create_dir_all(root.join("unrelated"))?;
        fs::write(root.join("AGENTS.md"), "ROOT_RULE")?;
        fs::write(root.join("src/AGENTS.md"), "SRC_RULE")?;
        fs::write(root.join("src/deep/AGENTS.md"), "DEEP_RULE")?;
        fs::write(root.join("unrelated/AGENTS.md"), "UNRELATED_RULE")?;
        fs::write(root.join("src/deep/mod.rs"), "pub fn sample() {}")?;
        let mut composer = PromptComposer::default();
        composer.refresh_for_task(&root, "Fix failing tests")?;
        let original = composer.initial_history(&root);
        let mut initial = original.clone();
        composer.apply(&mut initial);
        assert!(!initial[0].content.as_deref().unwrap().contains("DEEP_RULE"));

        // A file discovered via command output is enough, even if the user
        // never named the directory in the original request.
        assert!(composer.include_discovered_paths(&root, "src/deep/mod.rs:12:4")?);
        assert!(!composer.include_discovered_paths(&root, "src/deep/mod.rs")?);
        let mut updated = original.clone();
        composer.apply(&mut updated);
        let projected = updated[0].content.as_deref().unwrap();
        assert!(projected.contains("ROOT_RULE"));
        assert!(projected.contains("SRC_RULE"));
        assert!(projected.contains("DEEP_RULE"));
        assert!(!projected.contains("UNRELATED_RULE"));
        assert_eq!(projected.matches("DEEP_RULE").count(), 1);
        assert!(composer.extra_estimated_tokens(&original[0]) > 0);

        // A new task must discard dynamically discovered paths.
        composer.refresh_for_task(&root, "Fix another test")?;
        let mut next = original;
        composer.apply(&mut next);
        assert!(!next[0].content.as_deref().unwrap().contains("DEEP_RULE"));
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn nested_instructions_do_not_expose_windows_verbatim_prefix() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "kroma-nested-prefix-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir_all(root.join("pkg/sub"))?;
        fs::write(root.join("AGENTS.md"), "ROOT")?;
        fs::write(root.join("pkg/AGENTS.md"), "PKG_RULE")?;
        fs::write(root.join("pkg/sub/lib.rs"), "pub fn f() {}")?;
        let canonical_root = root.canonicalize()?;
        let mut composer = PromptComposer::default();
        composer.refresh_for_task(&canonical_root, "Inspect pkg")?;
        assert!(composer.include_discovered_paths(&canonical_root, "pkg/sub/lib.rs")?);
        let mut history = composer.initial_history(&canonical_root);
        composer.apply(&mut history);
        let text = history[0].content.as_deref().unwrap();
        assert!(text.contains("PKG_RULE"));
        assert!(
            !text.contains(r"\\?\"),
            "unexpected verbatim prefix in prompt: {text}"
        );
        fs::remove_dir_all(root)?;
        Ok(())
    }
}
