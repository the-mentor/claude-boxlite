//! Composition of the three ways a variable reaches a box.
//!
//! | mechanism        | value from       | guest sees  |
//! |------------------|------------------|-------------|
//! | passthrough list | host env, if set | real value  |
//! | -e KEY=VALUE     | the flag         | real value  |
//! | -e KEY           | host env, if set | real value  |
//! | --secret N=V@h   | host env var V   | placeholder |
//!
//! The last row is why `compose` refuses a variable that is both a secret
//! source and a passthrough entry: silently picking one is the difference
//! between a credential staying host-side and not, and it looks identical
//! from every network probe.

use anyhow::{Result, anyhow, bail};

/// Forwarded when set. Deliberately excludes GH_TOKEN/GITHUB_TOKEN, which
/// phase 1 moves to secrets, and TERM, which needs a guaranteed value and is
/// injected explicitly.
pub const DEFAULT_PASSTHROUGH: &[&str] = &[
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_MODEL",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "GIT_AUTHOR_NAME",
    "GIT_AUTHOR_EMAIL",
    "GIT_COMMITTER_NAME",
    "GIT_COMMITTER_EMAIL",
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "COLORTERM",
    "KITTY_WINDOW_ID",
    "WEZTERM_EXECUTABLE",
    "ITERM_SESSION_ID",
    "WT_SESSION",
    "VTE_VERSION",
];

/// `KEY=VALUE` → (KEY, Some(VALUE)); bare `KEY` → (KEY, None).
pub fn parse_e_flag(s: &str) -> Result<(String, Option<String>)> {
    match s.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.to_string(), Some(v.to_string()))),
        Some(_) => Err(anyhow!("-e with an empty variable name: {s:?}")),
        None if !s.is_empty() => Ok((s.to_string(), None)),
        None => Err(anyhow!("-e requires a variable name")),
    }
}

/// Build the plain (non-secret) environment for the box.
pub fn compose(
    e_flags: &[String],
    passthrough: &[String],
    secret_vars: &[String],
) -> Result<Vec<(String, String)>> {
    let mut out: Vec<(String, String)> = Vec::new();

    // Passthrough first, so an -e flag can override it below.
    for key in passthrough {
        if secret_vars.iter().any(|s| s == key) {
            bail!(
                "{key} is named as a secret source and is also in the passthrough list. \
                 Passing it through would put the real value in the guest, which is exactly \
                 what the secret exists to prevent. Remove it from one of the two."
            );
        }
        if let Ok(val) = std::env::var(key) {
            if !val.is_empty() {
                out.push((key.clone(), val));
            }
        }
    }

    for flag in e_flags {
        let (key, value) = parse_e_flag(flag)?;
        if secret_vars.iter().any(|s| *s == key) {
            bail!(
                "{key} is named as a secret source and cannot also be set with -e. \
                 The guest must see the placeholder, not a value."
            );
        }
        let resolved = match value {
            Some(v) => Some(v),
            None => std::env::var(&key).ok().filter(|v| !v.is_empty()),
        };
        let Some(resolved) = resolved else { continue };
        match out.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = resolved,
            None => out.push((key, resolved)),
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{compose, parse_e_flag};

    #[test]
    fn parses_key_value_and_bare_key() {
        assert_eq!(parse_e_flag("A=1").unwrap(), ("A".into(), Some("1".into())));
        assert_eq!(parse_e_flag("A").unwrap(), ("A".into(), None));
    }

    #[test]
    fn value_may_contain_equals_signs() {
        assert_eq!(
            parse_e_flag("URL=http://x/?a=b").unwrap(),
            ("URL".into(), Some("http://x/?a=b".into()))
        );
    }

    #[test]
    fn empty_key_is_rejected() {
        assert!(parse_e_flag("=value").is_err());
        assert!(parse_e_flag("").is_err());
    }

    #[test]
    fn explicit_value_beats_the_passthrough_list() {
        unsafe { std::env::set_var("CBOX_T_OVERRIDE", "from-host") };
        let out = compose(
            &["CBOX_T_OVERRIDE=from-flag".to_string()],
            &["CBOX_T_OVERRIDE".to_string()],
            &[],
        )
        .unwrap();
        assert_eq!(out, vec![("CBOX_T_OVERRIDE".to_string(), "from-flag".to_string())]);
    }

    #[test]
    fn unset_variables_are_skipped_not_passed_empty() {
        unsafe { std::env::remove_var("CBOX_T_ABSENT") };
        let out = compose(&[], &["CBOX_T_ABSENT".to_string()], &[]).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn bare_e_flag_forwards_from_the_host() {
        unsafe { std::env::set_var("CBOX_T_BARE", "v") };
        let out = compose(&["CBOX_T_BARE".to_string()], &[], &[]).unwrap();
        assert_eq!(out, vec![("CBOX_T_BARE".to_string(), "v".to_string())]);
    }

    #[test]
    fn a_secret_source_must_not_also_be_passed_through() {
        unsafe { std::env::set_var("CBOX_T_SECRET", "real-token") };
        let err = compose(&[], &["CBOX_T_SECRET".to_string()], &["CBOX_T_SECRET".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("CBOX_T_SECRET"), "error names the variable: {err}");
    }

    #[test]
    fn a_secret_source_must_not_be_overridden_by_an_e_flag_either() {
        let err = compose(
            &["CBOX_T_SECRET2=x".to_string()],
            &[],
            &["CBOX_T_SECRET2".to_string()],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("CBOX_T_SECRET2"), "error names the variable: {err}");
    }
}
