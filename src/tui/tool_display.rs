//! Human-readable tool activity, derived solely from the actual tool call.
//! These descriptions do not alter execution or infer an outcome from stderr.
use super::preferences::Language;

pub(super) fn prose_width(width: usize) -> usize {
    width.min((width.saturating_mul(3) / 4).clamp(100, 160))
}

/// Make one physical line, with a stable bound even for huge inline scripts.
/// Full commands and results stay in the expandable transcript.
fn first_line(value: &str, max_chars: usize) -> String {
    let first = value
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim();
    let mut snippet = first.chars().take(max_chars + 1).collect::<String>();
    if snippet.chars().count() > max_chars {
        snippet = snippet.chars().take(max_chars.saturating_sub(1)).collect();
        snippet.push('…');
    }
    snippet
}

fn executable(word: &str) -> &str {
    let trimmed = word.trim_matches(|c: char| c == '"' || c == '\'');
    trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed)
}

/// Recognize only a literal file target passed as the first argument to a
/// Node write API. This describes the visible command, never asserts that the
/// file was successfully written.
fn node_write_target(command: &str) -> Option<&str> {
    let args = command
        .split_once("writeFileSync(")
        .or_else(|| command.split_once("writeFile("))?
        .1
        .trim_start();
    let quote = args.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let tail = &args[quote.len_utf8()..];
    let target = tail.split_once(quote)?.0;
    (!target.is_empty() && target.len() <= 120 && !target.contains(['\r', '\n'])).then_some(target)
}

pub(super) fn summary(name: &str, command: &str, language: Language) -> String {
    if name != "shell" {
        return format!("{name} · {}", first_line(command, 64));
    }

    // Only skip simple leading cd...&& prefixes. This is for display, not
    // shell parsing; an unknown compound action keeps its actual first line.
    let mut action = command.trim();
    while action.starts_with("cd ") {
        let Some((_, tail)) = action.split_once("&&") else {
            break;
        };
        action = tail.trim_start();
    }
    let line = first_line(action, 100);
    let mut words = line.split_whitespace();
    let first = words.next().unwrap_or("");
    let second = words.next().unwrap_or("");
    let third = words.next().unwrap_or("");
    let program = executable(first);
    let program = program
        .strip_suffix(".exe")
        .unwrap_or(program)
        .to_ascii_lowercase();
    let caption = |en: &str, zh: &str| language.text(en, zh).to_owned();

    match (program.as_str(), second) {
        ("cargo", "test") => return caption("Run Rust tests", "运行 Rust 测试"),
        ("cargo", "clippy") => return caption("Check Rust lints", "检查 Rust 代码规范"),
        ("cargo", "fmt") => return caption("Format / check Rust code", "格式化或检查 Rust 代码"),
        ("cargo", "check" | "build") => {
            return caption("Build / check Rust project", "构建或检查 Rust 项目")
        }
        ("git", "status") => return caption("Inspect workspace changes", "检查工作区修改"),
        ("git", "diff") => return caption("Inspect code differences", "查看代码差异"),
        ("git", "add" | "commit" | "push" | "pull" | "fetch") => return format!("git {second}"),
        ("rg" | "grep" | "findstr", _) => {
            let search = first_line(action, 42);
            return format!("{} · {search}", language.text("Search files", "搜索文件"));
        }
        ("cat" | "sed" | "head" | "tail" | "ls" | "dir" | "pwd", _) if !line.contains('>') => {
            return format!(
                "{} · {}",
                language.text("Inspect", "查看"),
                first_line(action, 48)
            )
        }
        ("node" | "nodejs", "-v" | "--version") => {
            return caption("Check Node version", "检查 Node 版本")
        }
        ("node" | "nodejs", "-e" | "--eval") => {
            if let Some(target) = node_write_target(command) {
                return format!("{} · {target}", language.text("Write", "写入"));
            }
            return caption("Run inline Node script", "执行 Node 内联脚本");
        }
        ("node" | "nodejs", "--check") => {
            return format!(
                "{} · {}",
                language.text("Check JavaScript", "检查 JavaScript"),
                third
            )
        }
        ("node" | "nodejs", _) => {
            let target = if second.starts_with('-') || second.is_empty() {
                first_line(action, 55)
            } else {
                first_line(second, 55)
            };
            return format!("{} · {target}", language.text("Run Node", "运行 Node"));
        }
        ("npm" | "pnpm" | "yarn", "test") => {
            return format!("{program} · {}", language.text("Run tests", "运行测试"))
        }
        ("npm" | "pnpm" | "yarn", "run") => return format!("{program} · {third}"),
        ("npm" | "pnpm" | "yarn", _) => return format!("{program} · {second}"),
        ("python" | "python3" | "py", "-c") => {
            return caption("Run inline Python script", "执行 Python 内联脚本")
        }
        ("python" | "python3" | "py", _) => {
            let target = if second.starts_with('-') || second.is_empty() {
                first_line(action, 48)
            } else {
                first_line(second, 48)
            };
            return format!("{} · {target}", language.text("Run Python", "运行 Python"));
        }
        ("pwsh" | "powershell", _) => {
            return format!(
                "PowerShell · {}",
                first_line(
                    action.strip_prefix(first).unwrap_or(action).trim_start(),
                    58
                )
            );
        }
        ("set-content" | "out-file" | "add-content" | "new-item", _) => {
            return format!(
                "{} · {}",
                language.text("Write file", "写入文件"),
                first_line(action, 52)
            )
        }
        ("cp" | "copy" | "copy-item" | "mv" | "move" | "move-item", _) => {
            return format!(
                "{} · {}",
                language.text("Copy / move", "复制或移动"),
                first_line(action, 48)
            )
        }
        ("mkdir", _) => {
            return format!(
                "{} · {}",
                language.text("Create", "创建"),
                first_line(action, 50)
            )
        }
        ("bash" | "sh", "-c" | "-lc") => return caption("Run shell script", "执行 Shell 脚本"),
        _ => {}
    }
    if (program == "cat" && second == ">") || (program == "echo" && second == ">") {
        return format!("{} · {}", language.text("Write file", "写入文件"), third);
    }
    // No fabricated "purpose": show the real command prefix rather than
    // repeating the useless "Run shell command" label seven times.
    if line.is_empty() {
        return caption("Run command", "执行命令");
    }
    format!(
        "{} · {}",
        language.text("Run", "执行"),
        first_line(action, 58)
    )
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
    fn familiar_commands_are_named_without_claiming_results() {
        assert_eq!(
            summary(
                "shell",
                "cd /project && cargo build --release",
                Language::Zh
            ),
            "构建或检查 Rust 项目"
        );
        assert_eq!(
            summary("shell", "git diff --stat", Language::En),
            "Inspect code differences"
        );
        assert_eq!(summary("shell", "node -v", Language::Zh), "检查 Node 版本");
        assert_eq!(
            summary("shell", "node -e \"console.log(2)\"", Language::Zh),
            "执行 Node 内联脚本"
        );
        assert_eq!(
            summary("shell", "node build-svg.js", Language::Zh),
            "运行 Node · build-svg.js"
        );
        assert_eq!(
            summary("shell", "npm run build", Language::Zh),
            "npm · build"
        );
        assert_eq!(
            summary(
                "shell",
                "node -e \"require('fs').writeFileSync('bouncing-cat.svg','<svg/>')\"",
                Language::Zh
            ),
            "写入 · bouncing-cat.svg"
        );
        assert_eq!(
            summary("shell", "cat > drawing.svg <<'EOF'\n<svg>…", Language::Zh),
            "写入文件 · drawing.svg"
        );
        assert_eq!(
            summary("shell", "cargo testing", Language::En),
            "Run · cargo testing"
        );
        assert!(summary("mcp.read", "{\"file\":\"abc\"}", Language::En).contains("mcp.read"));
        let lengthy = "printf ".to_owned() + &"x".repeat(4000);
        assert!(summary("shell", &lengthy, Language::En).chars().count() <= 80);
        assert!(!summary("shell", &lengthy, Language::En).contains('\n'));
    }

    #[test]
    fn explicit_diagnostics_not_stderr_progress() {
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
