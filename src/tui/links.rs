//! Click targets for transcript text. Nothing runs until the user explicitly
//! Ctrl-clicks; no URL/file scanning occurs in the Agent's model context.
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Region {
    pub row: usize,
    pub start: usize,
    pub end: usize,
    pub target: String,
}

fn trim_start_delimiter(ch: char) -> bool {
    matches!(ch, '`' | '\'' | '"' | '(' | '[' | '<' | '{' | '【')
}
fn trim_end_delimiter(ch: char) -> bool {
    matches!(
        ch,
        '`' | '\'' | '"' | ')' | ']' | '>' | '}' | '】' | ',' | '.' | ';' | '，' | '。' | '；'
    )
}

/// A display candidate is deliberately strict; arbitrary prose is not
/// clickable. Clicking a missing local path returns a helpful error.
pub fn normalize_target(text: &str) -> Option<String> {
    let text = text.trim();
    if text.starts_with("http://") || text.starts_with("https://") {
        return reqwest::Url::parse(text)
            .ok()
            .filter(|url| matches!(url.scheme(), "http" | "https"))
            .map(|_| text.to_owned());
    }
    if text.starts_with("www.") && text.contains('.') {
        return Some(format!("https://{text}"));
    }
    if text.starts_with("file://") {
        return reqwest::Url::parse(text)
            .ok()
            .filter(|url| url.to_file_path().is_ok())
            .map(|_| text.to_owned());
    }
    let path = strip_location(text);
    let has_path_separator = path.contains('/') || path.contains('\\');
    let extension = Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let file_extension = matches!(
        extension.as_str(),
        "rs" | "md"
            | "txt"
            | "toml"
            | "json"
            | "yml"
            | "yaml"
            | "py"
            | "js"
            | "jsx"
            | "ts"
            | "tsx"
            | "go"
            | "java"
            | "c"
            | "cpp"
            | "h"
            | "hpp"
            | "html"
            | "css"
            | "sh"
            | "ps1"
            | "log"
            | "pdf"
            | "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
            | "csv"
            | "zip"
            | "docx"
            | "xlsx"
    );
    if file_extension
        || (has_path_separator
            && (path.starts_with("./")
                || path.starts_with("../")
                || path.starts_with(".\\")
                || path.starts_with("..\\")
                || path.starts_with("~/")
                || path.starts_with('/')
                || is_drive_path(path)))
    {
        Some(text.to_owned())
    } else {
        None
    }
}

/// Recognize compiler diagnostics like src/main.rs:42:3 without treating
/// the Windows drive's initial colon as a line number.
fn strip_location(value: &str) -> &str {
    let mut path = value;
    for _ in 0..2 {
        if let Some((prefix, suffix)) = path.rsplit_once(':') {
            if !prefix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit()) {
                path = prefix;
                continue;
            }
        }
        break;
    }
    path
}

fn is_drive_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
}

/// Produce clickable cell ranges from a *visible* row. Markdown links with
/// distinct labels also carry explicit targets from the Markdown renderer.
pub fn detect_visible(row: usize, line: &str) -> Vec<Region> {
    let mut regions = Vec::new();
    let mut index = 0;
    while index < line.len() {
        let ch = line[index..].chars().next().unwrap();
        if ch.is_whitespace() {
            index += ch.len_utf8();
            continue;
        }
        let begin = index;
        let quote = if matches!(ch, '`' | '"' | '\'') {
            Some(ch)
        } else {
            None
        };
        if let Some(quote) = quote {
            index += quote.len_utf8();
            while index < line.len() {
                let next = line[index..].chars().next().unwrap();
                index += next.len_utf8();
                if next == quote {
                    break;
                }
            }
        } else {
            while index < line.len() {
                let next = line[index..].chars().next().unwrap();
                if next.is_whitespace() || (index > begin && matches!(next, '，' | '。' | '；'))
                {
                    break;
                }
                index += next.len_utf8();
            }
        }
        let token = &line[begin..index];
        let prefix = token.trim_start_matches(trim_start_delimiter);
        let candidate = prefix.trim_end_matches(trim_end_delimiter);
        if let Some(target) = normalize_target(candidate) {
            let actual_start = begin + token.len() - prefix.len();
            let actual_end = actual_start + candidate.len();
            let start = UnicodeWidthStr::width(&line[..actual_start]);
            let end = UnicodeWidthStr::width(&line[..actual_end]);
            if end > start {
                regions.push(Region {
                    row,
                    start,
                    end,
                    target,
                });
            }
        }
    }
    regions
}

fn local_path(value: &str, workdir: &Path) -> Result<PathBuf> {
    let raw = if value.starts_with("file://") {
        let url = reqwest::Url::parse(value).context("invalid file URL")?;
        url.to_file_path()
            .map_err(|_| anyhow::anyhow!("invalid local file URL"))?
    } else {
        let value = strip_location(value);
        let value = if let Some(tail) = value.strip_prefix("~/") {
            std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                .map(|home| PathBuf::from(home).join(tail))
                .unwrap_or_else(|| PathBuf::from(value))
        } else {
            PathBuf::from(value)
        };
        value
    };
    let resolved = if raw.is_absolute() {
        raw
    } else {
        workdir.join(raw)
    };
    resolved
        .canonicalize()
        .with_context(|| format!("file not found: {}", resolved.display()))
}

/// Use an OS default handler without constructing a shell command from
/// model-produced text. Only explicit user clicks reach this function.
pub fn open_target(value: &str, workdir: &Path) -> Result<()> {
    let destination = if value.starts_with("http://")
        || value.starts_with("https://")
        || value.starts_with("www.")
    {
        let normalized = normalize_target(value).context("invalid web address")?;
        let url = reqwest::Url::parse(&normalized).context("invalid web address")?;
        if !matches!(url.scheme(), "http" | "https") {
            bail!("only HTTP and HTTPS addresses can be opened");
        }
        std::ffi::OsString::from(url.as_str())
    } else {
        local_path(value, workdir)?.into_os_string()
    };
    open_with_system(&destination)
}

#[cfg(windows)]
fn open_with_system(value: &std::ffi::OsStr) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;

    let wide: Vec<u16> = value.encode_wide().chain(std::iter::once(0)).collect();
    let verb: Vec<u16> = "open".encode_utf16().chain(std::iter::once(0)).collect();
    // ShellExecute receives an argument, not interpolated PowerShell/cmd
    // source. Paths with quotes, spaces, ampersands remain data.
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            wide.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
        )
    } as isize;
    if result <= 32 {
        bail!("Windows could not open this target (ShellExecute code {result})");
    }
    Ok(())
}

#[cfg(not(windows))]
fn open_with_system(value: &std::ffi::OsStr) -> Result<()> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(program)
        .arg(value)
        .spawn()
        .with_context(|| format!("starting {program}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_urls_windows_paths_unicode_and_compiler_locations() {
        let text =
            "报告 https://example.org/a?x=1 和 src/provider.rs:128，以及 D:/Kroma/docs/方案.md";
        let regions = detect_visible(3, text);
        assert_eq!(regions.len(), 3);
        assert_eq!(regions[0].target, "https://example.org/a?x=1");
        assert_eq!(regions[1].target, "src/provider.rs:128");
        assert_eq!(regions[2].target, "D:/Kroma/docs/方案.md");
        assert_eq!(regions[0].start, UnicodeWidthStr::width("报告 "));
    }

    #[test]
    fn trims_punctuation_and_accepts_quoted_paths_with_spaces() {
        let regions = detect_visible(0, r#"请见 [https://example.org]. "C:/My Docs/report.md"。"#);
        assert_eq!(regions.len(), 2);
        assert_eq!(regions[0].target, "https://example.org");
        assert_eq!(regions[1].target, "C:/My Docs/report.md");
        assert!(normalize_target("please").is_none());
        assert!(normalize_target("foo;bar").is_none());
    }

    #[test]
    fn strips_line_and_column_but_not_the_windows_drive_colon() {
        assert_eq!(strip_location("src/lib.rs:42:7"), "src/lib.rs");
        assert_eq!(strip_location("C:/Kroma/main.rs:20"), "C:/Kroma/main.rs");
        assert_eq!(strip_location("C:/Kroma/main.rs"), "C:/Kroma/main.rs");
    }

    #[test]
    fn nonexistent_local_path_cannot_be_launched() {
        let result = local_path("no-such-kroma-file.md", std::env::temp_dir().as_path());
        assert!(result.is_err());
    }
}
