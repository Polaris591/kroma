//! Display-only hints. Never use these heuristics to determine execution status.
use super::preferences::Language;

pub(super) fn prose_width(width: usize) -> usize {
    width.min((width.saturating_mul(3) / 4).clamp(100, 160))
}

pub(super) fn summary<'a>(name: &str, command: &'a str, language: Language) -> &'a str {
    if name != "shell" {
        return command;
    }
    // Only recognize simple leading directory changes. This is not a shell
    // parser: unknown scripts get a neutral label, not a guessed intent.
    let mut action = command.trim();
    while action.starts_with("cd ") {
        let Some((_, rest)) = action.split_once("&&") else {
            break;
        };
        action = rest.trim_start();
    }
    let words: Vec<_> = action.split_whitespace().take(2).collect();
    match words.as_slice() {
        ["cargo", "test"] => language.text("Run Rust tests", "运行 Rust 测试"),
        ["cargo", "clippy"] => language.text("Check Rust lints", "检查 Rust 代码规范"),
        ["cargo", "fmt"] => language.text("Format / check Rust code", "格式化或检查 Rust 代码"),
        ["cargo", "check" | "build"] => {
            language.text("Build / check Rust project", "构建或检查 Rust 项目")
        }
        ["git", "status"] => language.text("Inspect workspace changes", "检查工作区修改"),
        ["git", "diff"] => language.text("Inspect code differences", "查看代码差异"),
        ["rg" | "grep", _] => language.text("Search project files", "搜索项目文件"),
        ["cat" | "sed" | "head" | "ls" | "pwd", _] if !action.contains('>') => {
            language.text("Inspect files / workspace", "查看文件或工作区")
        }
        _ => language.text(
            "Run shell command · Ctrl+O details",
            "执行 Shell 命令 · Ctrl+O 查看详情",
        ),
    }
}

pub(super) fn has_diagnostic(details: &str) -> bool {
    details.lines().any(|line| {
        let line = line
            .trim()
            .strip_prefix("stderr: ")
            .unwrap_or(line.trim())
            .trim_start()
            .to_ascii_lowercase();
        line.starts_with("warning:")
            || line.starts_with("warning[")
            || line.starts_with("error:")
            || line.starts_with("error[")
            || line.starts_with("fatal:")
            || line.starts_with("警告：")
            || line.starts_with("错误：")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compound_commands_have_short_honest_labels() {
        assert_eq!(
            summary(
                "shell",
                "cd /project && cargo build --release",
                Language::Zh
            ),
            "构建或检查 Rust 项目"
        );
        assert_eq!(
            summary("shell", "cargo testing", Language::En),
            "Run shell command · Ctrl+O details"
        );
        assert_eq!(
            summary("shell", "cat > deploy.ps1 <<'EOF'\ncommands", Language::Zh),
            "执行 Shell 命令 · Ctrl+O 查看详情"
        );
    }
    #[test]
    fn cargo_progress_is_not_a_warning_but_explicit_diagnostics_are() {
        assert!(!has_diagnostic(
            "exit 0\nstderr:    Compiling kroma\n    Finished release profile"
        ));
        assert!(has_diagnostic("exit 0\nstderr: warning: unused variable"));
        assert!(has_diagnostic("exit 0\nfatal: not a git repository"));
        assert!(!has_diagnostic("exit 0\n0 errors, 0 warnings"));
        assert_eq!(prose_width(80), 80);
        assert_eq!(prose_width(180), 135);
        assert_eq!(prose_width(240), 160);
    }
}
