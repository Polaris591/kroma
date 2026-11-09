//! One catalog for TUI command descriptions, search and completion.
use super::{picker::Item, preferences::Language};

pub(super) struct Command {
    pub name: &'static str,
    pub args: &'static str,
    en: &'static str,
    zh: &'static str,
}

impl Command {
    pub fn description(&self, language: Language) -> &str {
        language.text(self.en, self.zh)
    }
}

macro_rules! commands {
    ($(($name:literal, $args:literal, $en:literal, $zh:literal)),* $(,)?) => {
        pub(super) const COMMANDS: &[Command] = &[$(Command { name: $name, args: $args, en: $en, zh: $zh }),*];
    };
}

commands![
    (
        "/help",
        "",
        "Shortcuts, search and the command index",
        "快捷键、搜索与命令索引"
    ),
    (
        "/new",
        "",
        "Welcome page; start a new conversation",
        "返回欢迎页，开始新会话"
    ),
    (
        "/clear",
        "",
        "Clear the current conversation",
        "清空当前会话"
    ),
    (
        "/compact",
        "[focus]",
        "Compact context now; original history is preserved",
        "立即压缩上下文，保留原始会话记录"
    ),
    (
        "/sessions",
        "[id|rename <name>]",
        "Browse, resume or rename conversations",
        "浏览、恢复或重命名会话"
    ),
    (
        "/model",
        "[status|profile]",
        "Choose a model profile, or show resolved limits",
        "选择模型配置，或查看生效的模型限制"
    ),
    ("/think", "[level]", "Set reasoning effort", "设置推理强度"),
    ("/exit", "", "Quit Kroma", "退出 Kroma"),
    (
        "/language",
        "[en|zh]",
        "Switch interface language (saved)",
        "切换界面语言并保存"
    ),
    (
        "/tools",
        "",
        "List tools, skills and MCP warnings",
        "查看工具、技能与 MCP 警告"
    ),
    (
        "/plugins",
        "[list|enable|disable|install|config|set|reset|view|reload]",
        "Manage plugins",
        "管理插件"
    ),
    (
        "/usage",
        "",
        "Show session token usage and cost",
        "查看本次会话的用量与费用"
    ),
    (
        "/checkpoints",
        "[list|restore <n>|fork <n>|rewind <n>|history]",
        "File snapshots: list, restore, fork, rewind, history",
        "文件快照：列表、恢复、分叉、回退、历史"
    ),
    (
        "/queue",
        "[list|clear|run|<text>]",
        "Queued tasks: list, clear, run, or add one",
        "排队任务：查看、清空、执行或新增"
    ),
    (
        "/transcript",
        "[export|tool <id>]",
        "Searchable transcript, export and raw tool output",
        "可搜索的全文记录、导出与原始工具输出"
    ),
];

pub(super) fn items_with_plugins(language: Language, plugins: &[(String, String)]) -> Vec<Item> {
    let mut items = items(language);
    for (name, description) in plugins {
        if !COMMANDS.iter().any(|command| command.name == name)
            && !items.iter().any(|item| item.value == *name)
        {
            items.push(Item::new(name, name, description));
        }
    }
    items
}

pub(super) fn items(language: Language) -> Vec<Item> {
    COMMANDS
        .iter()
        .map(|command| {
            Item::new(
                format!("{} {}", command.name, command.args)
                    .trim()
                    .to_owned(),
                command.name,
                command.description(language),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every catalog name must be dispatchable by some host. A name that no
    /// registry and no full-screen handler owns is a broken promise to users.
    #[test]
    fn every_catalog_name_is_dispatchable() {
        let commands = crate::commands::CommandRegistry::builtin(Vec::new()).unwrap();
        for command in COMMANDS {
            let name = command.name;
            let plugin_owned = name == "/goal" || name.starts_with("/plugin");
            assert!(
                commands.knows(name) || crate::commands::is_full_screen_only(name) || plugin_owned,
                "catalog lists {name}, but no host dispatches it"
            );
        }
    }

    #[test]
    fn enabled_plugin_commands_appear_once_in_completion() {
        let commands = items_with_plugins(
            Language::En,
            &[
                ("/echo".into(), "Example command".into()),
                ("/help".into(), "Must not override the built-in".into()),
            ],
        );
        assert_eq!(
            commands.iter().filter(|item| item.value == "/echo").count(),
            1
        );
        assert_eq!(
            commands.iter().filter(|item| item.value == "/help").count(),
            1
        );
    }

    #[test]
    fn catalog_names_are_unique_and_descriptions_are_bilingual() {
        let mut names = std::collections::HashSet::new();
        for cmd in COMMANDS {
            assert!(names.insert(cmd.name), "duplicate {}", cmd.name);
            assert!(!cmd.description(Language::En).is_empty());
            assert!(!cmd.description(Language::Zh).is_empty());
            assert_ne!(cmd.description(Language::En), cmd.description(Language::Zh));
        }
        assert!(items(Language::Zh)
            .iter()
            .any(|i| i.value == "/language" && i.hint.contains("语言")));
    }
}
