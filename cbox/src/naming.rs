//! Box-name resolution.
//!
//! The default name is derived so it disappears from the UX: `cbox exec` in a
//! directory finds that directory's box without being told. Deriving from the
//! git root rather than the cwd is what makes that work from a subdirectory.

use std::path::Path;
use std::process::Command;

/// Which rule produced a name. Surfaced by `cbox name` because a derived
/// default is magic and magic has to be inspectable.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NameSource {
    Explicit,
    EnvVar,
    GitRoot,
    Cwd,
    Fallback,
}

impl NameSource {
    pub fn describe(self) -> &'static str {
        match self {
            NameSource::Explicit => "explicit argument",
            NameSource::EnvVar => "CBOX_NAME",
            NameSource::GitRoot => "git repo root",
            NameSource::Cwd => "current directory",
            NameSource::Fallback => "built-in default",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedName {
    pub name: String,
    pub source: NameSource,
}

pub const FALLBACK: &str = "claude-box";
const MAX_LEN: usize = 64;

/// Make a string safe to use as a directory name under `boxes/<name>/`.
pub fn sanitize(raw: &str) -> String {
    let mapped: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();

    let mut out = String::with_capacity(mapped.len());
    for c in mapped.chars() {
        if c == '-' && out.ends_with('-') {
            continue;
        }
        out.push(c);
    }

    out.truncate(MAX_LEN);
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        FALLBACK.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Resolve the box name by precedence: explicit > CBOX_NAME > git root >
/// cwd > fallback.
pub fn resolve(explicit: Option<&str>, cwd: &Path) -> ResolvedName {
    if let Some(name) = explicit {
        return ResolvedName { name: sanitize(name), source: NameSource::Explicit };
    }
    if let Ok(name) = std::env::var("CBOX_NAME") {
        if !name.trim().is_empty() {
            return ResolvedName { name: sanitize(&name), source: NameSource::EnvVar };
        }
    }
    if let Some(root) = git_root(cwd) {
        if let Some(base) = root.file_name().and_then(|s| s.to_str()) {
            return ResolvedName { name: sanitize(base), source: NameSource::GitRoot };
        }
    }
    if let Some(base) = cwd.file_name().and_then(|s| s.to_str()) {
        return ResolvedName { name: sanitize(base), source: NameSource::Cwd };
    }
    ResolvedName { name: FALLBACK.to_string(), source: NameSource::Fallback }
}

fn git_root(cwd: &Path) -> Option<std::path::PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8(out.stdout).ok()?;
    let path = path.trim();
    if path.is_empty() { None } else { Some(std::path::PathBuf::from(path)) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn sanitize_passes_through_already_valid_names() {
        assert_eq!(sanitize("claude-boxlite"), "claude-boxlite");
        assert_eq!(sanitize("my_box.v2"), "my_box.v2");
    }

    #[test]
    fn sanitize_replaces_illegal_characters() {
        assert_eq!(sanitize("foo.bar/baz"), "foo.bar-baz");
        assert_eq!(sanitize("a b c"), "a-b-c");
        assert_eq!(sanitize("emoji🎉here"), "emoji-here");
    }

    #[test]
    fn sanitize_collapses_and_trims_dashes() {
        assert_eq!(sanitize("a///b"), "a-b");
        assert_eq!(sanitize("--lead-and-trail--"), "lead-and-trail");
        assert_eq!(sanitize("///"), FALLBACK);
    }

    #[test]
    fn sanitize_truncates_to_max_len_without_trailing_dash() {
        let long = "x".repeat(200);
        assert_eq!(sanitize(&long).len(), 64);
        // Truncation must not leave a dangling separator.
        let awkward = format!("{}-{}", "y".repeat(63), "z".repeat(50));
        assert!(!sanitize(&awkward).ends_with('-'));
    }

    #[test]
    fn sanitize_empty_falls_back() {
        assert_eq!(sanitize(""), FALLBACK);
        assert_eq!(sanitize("   "), FALLBACK);
    }

    #[test]
    fn explicit_name_wins_and_is_still_sanitized() {
        let _lock = crate::test_env_lock::lock();
        unsafe { std::env::remove_var("CBOX_NAME") };
        let r = resolve(Some("My Box"), &PathBuf::from("/tmp"));
        assert_eq!(r.source, NameSource::Explicit);
        assert_eq!(r.name, "My-Box");
    }

    #[test]
    fn cwd_basename_is_used_when_not_in_a_repo() {
        let _lock = crate::test_env_lock::lock();
        unsafe { std::env::remove_var("CBOX_NAME") };
        // /tmp is not a git repo on any machine this runs on.
        let r = resolve(None, &PathBuf::from("/tmp"));
        assert_eq!(r.source, NameSource::Cwd);
        assert_eq!(r.name, "tmp");
    }

    #[test]
    fn root_directory_falls_back() {
        let _lock = crate::test_env_lock::lock();
        unsafe { std::env::remove_var("CBOX_NAME") };
        let r = resolve(None, &PathBuf::from("/"));
        assert_eq!(r.source, NameSource::Fallback);
        assert_eq!(r.name, FALLBACK);
    }

    #[test]
    fn env_var_is_used_when_set_and_no_explicit_name() {
        let _lock = crate::test_env_lock::lock();
        unsafe { std::env::set_var("CBOX_NAME", "From Env") };
        let r = resolve(None, &PathBuf::from("/tmp"));
        assert_eq!(r.source, NameSource::EnvVar);
        assert_eq!(r.name, "From-Env");
        unsafe { std::env::remove_var("CBOX_NAME") };
    }

    #[test]
    fn git_root_name_is_used_and_shared_by_subdirectories() {
        let _lock = crate::test_env_lock::lock();
        unsafe { std::env::remove_var("CBOX_NAME") };

        let repo = std::env::temp_dir().join(format!("cbox-naming-test-{}", std::process::id()));
        let subdir = repo.join("nested").join("deeper");
        std::fs::create_dir_all(&subdir).expect("create temp repo tree");
        let status = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .arg("init")
            .arg("--quiet")
            .status()
            .expect("run git init");
        assert!(status.success(), "git init failed");

        // Canonicalize: /tmp is a symlink on macOS, and git reports the
        // resolved path as the toplevel, not the symlinked one.
        let repo = repo.canonicalize().expect("canonicalize repo path");
        let subdir = repo.join("nested").join("deeper");
        let expected_name = repo.file_name().and_then(|s| s.to_str()).unwrap().to_string();

        let at_root = resolve(None, &repo);
        assert_eq!(at_root.source, NameSource::GitRoot);
        assert_eq!(at_root.name, expected_name);

        // The whole point of deriving from the git root: a subdirectory
        // resolves to the same name, not its own basename ("deeper").
        let at_subdir = resolve(None, &subdir);
        assert_eq!(at_subdir.source, NameSource::GitRoot);
        assert_eq!(at_subdir.name, expected_name);

        std::fs::remove_dir_all(&repo).expect("clean up temp repo");
    }
}
