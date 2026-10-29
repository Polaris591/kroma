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
        "Keyboard shortcuts and help",
        "快捷键与使用帮助"
    ),
    (
        "/commands",
        "",
        "Search all slash commands",
        "搜索全部斜杠命令"
    ),
    (
        "/language",
        "[en|zh]",
        "Switch interface language (saved)",
        "切换界面语言并保存"
    ),
    (
        "/layout",
        "[comfortable|wide]",
        "Reading width: comfortable or full",
        "切换舒适阅读或全宽布局"
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
        "/sessions",
        "",
        "Browse saved conversations",
        "浏览已保存会话"
    ),
    (
        "/resume",
        "[session]",
        "Resume a saved conversation",
        "恢复指定会话"
    ),
    (
        "/continue",
        "",
        "Resume the latest conversation",
        "恢复最近会话"
    ),
    (
        "/rename",
        "<name>",
        "Rename this conversation",
        "重命名当前会话"
    ),
    (
        "/model",
        "[profile]",
        "Choose a model profile",
        "选择模型配置"
    ),
    ("/think", "[level]", "Set reasoning effort", "设置推理强度"),
    ("/reasoning", "[level]", "Alias for /think", "/think 的别名"),
    (
        "/theme",
        "[dark|light|mono|reload]",
        "Change or reload theme",
        "切换或重新加载主题"
    ),
    (
        "/tools",
        "",
        "List tools and MCP warnings",
        "查看工具与 MCP 警告"
    ),
    ("/skills", "", "List available skills", "查看可用技能"),
    (
        "/plugins",
        "[list|enable|disable|install|config|set|reset|view]",
        "Manage plugins (restart to apply)",
        "管理插件（重启后生效）"
    ),
    (
        "/usage",
        "",
        "Detailed usage and cache statistics",
        "查看详细用量与缓存统计"
    ),
    ("/diff", "", "Show workspace changes", "查看工作区修改"),
    (
        "/export",
        "<path>",
        "Export conversation to a file",
        "将会话导出到文件"
    ),
    (
        "/history",
        "[count]",
        "List conversation checkpoints",
        "查看会话检查点"
    ),
    (
        "/fork",
        "<checkpoint>",
        "Branch from a checkpoint",
        "从检查点创建分支"
    ),
    (
        "/rewind",
        "<checkpoint>",
        "Rewind and preserve old history",
        "回退并保留原历史"
    ),
    ("/undo", "", "Undo the last rewind", "撤销上次回退"),
    (
        "/pending",
        "",
        "Show queued tasks and steering",
        "查看待办任务与引导指令"
    ),
    (
        "/queue",
        "<task>|list|clear|run",
        "Manage follow-up tasks",
        "管理后续任务队列"
    ),
    (
        "/steer",
        "<instruction>",
        "Guide the active task",
        "引导正在执行的任务"
    ),
    (
        "/editor",
        "",
        "Edit draft in external editor",
        "使用外部编辑器编辑草稿"
    ),
    ("/exit", "", "Exit Kroma", "退出 Kroma"),
    ("/quit", "", "Alias for /exit", "/exit 的别名"),
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
            assert_ne!(cmd.description(Language::En), cmd.description(Language::Zh));
        }
        assert!(items(Language::Zh)
            .iter()
            .any(|i| i.value == "/language" && i.hint.contains("语言")));
    }
}
