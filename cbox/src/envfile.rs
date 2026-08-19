//! `.env`-style file loading, mirroring how `config::resolve_config_path`
//! resolves the registries file: an explicit path wins, then an env var,
//! then a fixed fallback under `~/.config/cbox`, and a missing file is not an
//! error.
//!
//! This is deliberately not `dotenvy`/`dotenv` — the format needed here is a
//! subset (`KEY=VALUE`, `#` comments, optional quoting) that's a couple of
//! dozen lines to parse directly, and pulling in a crate for it would be the
//! wrong side of the "does this need to exist" question.
//!
//! Never call anything in this module from anywhere that might log, print,
//! or otherwise surface a *value* — only key names are safe to mention.

use std::path::{Path, PathBuf};

/// `--env-file`, then `$CBOX_ENV_FILE`, then `~/.config/cbox/env`.
///
/// Never falls back to a `.env` in the current directory: `cbox` is meant to
/// run from any project, and silently slurping that project's own `.env`
/// would leak whatever secrets happen to live there into the box.
pub fn resolve_path(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p.to_path_buf());
    }
    if let Ok(p) = std::env::var("CBOX_ENV_FILE") {
        if !p.trim().is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/cbox/env"))
}

/// Parse `KEY=VALUE` lines. Blank lines and `#`-prefixed comments are
/// skipped; a line with no `=` or an empty key is skipped too, since one bad
/// line should not take down every variable after it.
fn parse(contents: &str) -> Vec<(String, String)> {
    contents
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            let key = key.trim();
            if key.is_empty() {
                return None;
            }
            Some((key.to_string(), unquote(value.trim())))
        })
        .collect()
}

/// Strip one layer of matching surrounding quotes, single or double.
fn unquote(v: &str) -> String {
    let bytes = v.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return v[1..v.len() - 1].to_string();
        }
    }
    v.to_string()
}

/// A missing file yields no variables rather than an error — same rule
/// `config::load_registries` follows for the registries file, so a bare
/// `cbox up` on a machine with no env file configured still boots.
fn load(path: &Path) -> Vec<(String, String)> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return vec![];
    };
    parse(&raw)
}

/// Load `path` and set each variable in the process environment, except any
/// key that is already set there.
///
/// That exception is the whole point: someone exporting a variable by hand
/// for a one-off run (`ANTHROPIC_MODEL=foo cbox up`) must not be silently
/// overwritten by whatever a config file says — the file is a fallback for
/// what the shell didn't already provide, not an override of it. This is the
/// opposite of what some dotenv libraries default to, hence writing this
/// explicitly rather than reaching for one.
pub fn apply(path: &Path) {
    for (key, value) in load(path) {
        if std::env::var_os(&key).is_none() {
            unsafe { std::env::set_var(&key, &value) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env_lock::EnvVarGuard;

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn parses_basic_lines_skipping_blanks_and_comments() {
        let vars = parse("A=1\n\n# a comment\nB=2\n");
        assert_eq!(vars, vec![("A".to_string(), "1".to_string()), ("B".to_string(), "2".to_string())]);
    }

    #[test]
    fn strips_surrounding_single_or_double_quotes() {
        let vars = parse("A=\"hello\"\nB='world'\nC=bare\n");
        assert_eq!(
            vars,
            vec![
                ("A".to_string(), "hello".to_string()),
                ("B".to_string(), "world".to_string()),
                ("C".to_string(), "bare".to_string()),
            ]
        );
    }

    #[test]
    fn a_malformed_line_is_skipped_not_fatal() {
        let vars = parse("GOOD=1\nthis has no equals\n=noKey\nALSO_GOOD=2\n");
        assert_eq!(
            vars,
            vec![("GOOD".to_string(), "1".to_string()), ("ALSO_GOOD".to_string(), "2".to_string())]
        );
    }

    #[test]
    fn value_may_contain_an_equals_sign() {
        let vars = parse("URL=http://x/?a=b\n");
        assert_eq!(vars, vec![("URL".to_string(), "http://x/?a=b".to_string())]);
    }

    #[test]
    fn missing_file_yields_no_variables_rather_than_an_error() {
        assert!(load(Path::new("/nonexistent/cbox-env-file-test")).is_empty());
    }

    #[test]
    fn explicit_path_wins_over_env_var_and_default() {
        let _lock = crate::test_env_lock::lock();
        let _cbox = EnvVarGuard::set("CBOX_ENV_FILE", "/from/env/var");
        let explicit = PathBuf::from("/from/flag");
        assert_eq!(resolve_path(Some(&explicit)), Some(explicit));
    }

    #[test]
    fn env_var_wins_over_the_default_when_no_explicit_path() {
        let _lock = crate::test_env_lock::lock();
        let _cbox = EnvVarGuard::set("CBOX_ENV_FILE", "/from/env/var");
        assert_eq!(resolve_path(None), Some(PathBuf::from("/from/env/var")));
    }

    #[test]
    fn falls_back_to_the_config_default_when_nothing_else_is_set() {
        let _lock = crate::test_env_lock::lock();
        let _cbox = EnvVarGuard::remove("CBOX_ENV_FILE");
        let resolved = resolve_path(None).unwrap();
        assert!(resolved.ends_with(".config/cbox/env"), "{}", resolved.display());
    }

    /// The core guarantee this feature exists for: a variable already set in
    /// the real process environment must win over the file, not the other
    /// way around. This is the opposite of some dotenv libraries' default,
    /// so it's asserted directly rather than assumed.
    #[test]
    fn the_process_environment_wins_over_the_file() {
        let _lock = crate::test_env_lock::lock();
        let _existing = EnvVarGuard::set("CBOX_T_ENVFILE_WINNER", "from-process");
        let _absent = EnvVarGuard::remove("CBOX_T_ENVFILE_LOSER");

        let dir = std::env::temp_dir().join("cbox-test-envfile-precedence");
        let p = write(
            &dir,
            "env",
            "CBOX_T_ENVFILE_WINNER=from-file\nCBOX_T_ENVFILE_LOSER=from-file\n",
        );

        apply(&p);

        assert_eq!(std::env::var("CBOX_T_ENVFILE_WINNER").unwrap(), "from-process");
        assert_eq!(std::env::var("CBOX_T_ENVFILE_LOSER").unwrap(), "from-file");

        unsafe { std::env::remove_var("CBOX_T_ENVFILE_LOSER") };
        let _ = std::fs::remove_file(&p);
    }
}
