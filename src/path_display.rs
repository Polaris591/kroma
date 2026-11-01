//! Path rendering for prompts, user-facing references and diagnostics.
//!
//! Windows `canonicalize()` returns a verbatim `\\?\C:\...` path. Native file
//! APIs accept it, but Git Bash and many command-line tools do not. Keep the
//! native path for filesystem calls; render a shell-friendly form only where
//! the path is shown to a user or placed in a model-visible instruction.

use std::path::{Path, PathBuf};

/// Render a path that a shell (Git Bash, PowerShell or cmd) can consume.
/// Windows paths drop the verbatim prefix and use forward slashes. Other
/// platforms keep their native spelling.
pub fn for_shell(path: &Path) -> String {
    #[cfg(windows)]
    {
        let mut text = path.to_string_lossy().replace('/', "\\");
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            text = format!(r"\\{rest}");
        } else if let Some(rest) = text.strip_prefix(r"\\?\") {
            text = rest.to_owned();
        }
        text.replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        path.to_string_lossy().into_owned()
    }
}

/// Compare a path with a root after normalizing Windows verbatim prefixes and
/// separators. Returns the suffix below `root`, if any.
pub fn strip_prefix_equivalent(path: &Path, root: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let path = windows_normalized(path);
        let root = windows_normalized(root);
        if path.eq_ignore_ascii_case(&root) {
            return Some(PathBuf::new());
        }
        let root = root.trim_end_matches('\\');
        let prefix = format!("{root}\\");
        if starts_with_ignore_ascii_case(&path, &prefix) {
            // The prefix is a byte-for-byte suffix of the normalized path up
            // to case. Slicing at the prefix length is correct for ASCII
            // prefixes, which covers drive, share and home paths.
            let rest = path.get(prefix.len()..)?;
            return Some(PathBuf::from(rest));
        }
        None
    }
    #[cfg(not(windows))]
    {
        path.strip_prefix(root).ok().map(Path::to_path_buf)
    }
}

#[cfg(windows)]
fn starts_with_ignore_ascii_case(text: &str, prefix: &str) -> bool {
    text.len() >= prefix.len()
        && text.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

#[cfg(windows)]
fn windows_normalized(path: &Path) -> String {
    let mut text = path.to_string_lossy().replace('/', "\\");
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        text = format!(r"\\{rest}");
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        text = rest.to_owned();
    }
    while text.len() > 1 && text.ends_with('\\') {
        text.pop();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn verbatim_paths_render_without_the_device_prefix() {
        assert_eq!(
            for_shell(Path::new(r"\\?\C:\Users\me\project")),
            "C:/Users/me/project"
        );
        let rest = strip_prefix_equivalent(
            Path::new(r"\\?\C:\Users\ME\project"),
            Path::new(r"C:\Users\me"),
        )
        .unwrap();
        assert_eq!(rest, PathBuf::from(r"project"));
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_paths_are_unchanged() {
        let path = Path::new("/home/me/project");
        assert_eq!(for_shell(path), "/home/me/project");
        assert_eq!(
            strip_prefix_equivalent(path, Path::new("/home/me")).unwrap(),
            PathBuf::from("project")
        );
    }
}
