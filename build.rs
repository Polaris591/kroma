use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_owned())
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // Git's HEAD may be a worktree file, not .git/HEAD. Track both the
    // active ref and HEAD so a new commit rebuilds the embedded revision.
    if let Some(head) = git(&["rev-parse", "--git-path", "HEAD"]) {
        println!("cargo:rerun-if-changed={head}");
    }
    if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]) {
        if let Some(path) = git(&["rev-parse", "--git-path", &branch]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    if let Some(index) = git(&["rev-parse", "--git-path", "index"]) {
        println!("cargo:rerun-if-changed={index}");
    }
    // Dirty state depends on worktree files, not just HEAD. Make Cargo
    // rerun this script when an existing tracked file changes (including
    // README and tests); the Git index catches added/removed tracked paths.
    // Never watch the whole repository: target/ changes on every build.
    if let Ok(output) = Command::new("git").args(["ls-files", "-z"]).output() {
        if output.status.success() {
            for raw in output
                .stdout
                .split(|byte| *byte == 0)
                .filter(|raw| !raw.is_empty())
            {
                if let Ok(path) = std::str::from_utf8(raw) {
                    if !path.contains('\n') {
                        println!("cargo:rerun-if-changed={path}");
                    }
                }
            }
        }
    }
    let sha = git(&["rev-parse", "--short=12", "HEAD"])
        .filter(|sha| !sha.is_empty() && sha.bytes().all(|c| c.is_ascii_hexdigit()))
        .unwrap_or_else(|| "source".to_owned());
    let dirty = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=normal"])
        .output()
        .ok()
        .is_some_and(|output| output.status.success() && !output.stdout.is_empty());
    let suffix = if dirty { "-dirty" } else { "" };
    println!("cargo:rustc-env=KROMA_BUILD_REVISION={sha}{suffix}");
}
