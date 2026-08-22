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

/// Forwarded unconditionally when set. Deliberately excludes
/// GH_TOKEN/GITHUB_TOKEN, which phase 1 moves to secrets, and TERM, which
/// needs a guaranteed value and is injected explicitly.
const UNCONDITIONAL_PASSTHROUGH: &[&str] = &[
    "ANTHROPIC_MODEL",
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

fn is_set_and_non_empty(key: &str) -> bool {
    std::env::var(key).map(|v| !v.is_empty()).unwrap_or(false)
}

/// Which Anthropic auth vars to forward: one of three mutually exclusive
/// sets, not a flat union. A subscription OAuth token wins and travels with
/// ANTHROPIC_BASE_URL if one is set (the
/// `/claude` passthrough route on the gateway) or goes direct if not.
/// Otherwise a set ANTHROPIC_BASE_URL means keyed-gateway mode via `/api`,
/// where the real API key must deliberately stay host-side — the gateway
/// injects it from its own environment, so ANTHROPIC_API_KEY is absent from
/// this branch's output on purpose, not by omission. Only when neither is
/// set (talking to the Anthropic API directly, no gateway involved) does the
/// raw key get forwarded.
fn llm_passthrough() -> Vec<&'static str> {
    if is_set_and_non_empty("CLAUDE_CODE_OAUTH_TOKEN") {
        vec!["CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_BASE_URL"]
    } else if is_set_and_non_empty("ANTHROPIC_BASE_URL") {
        vec!["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL"]
    } else {
        vec!["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"]
    }
}

/// The full passthrough list for this run: the LLM-auth branch selected by
/// what's currently set, plus the unconditional model/git/terminal vars.
/// Recomputed per call (not a `const`) because the LLM branch depends on the
/// live environment, which an env-file load can change between calls.
pub fn passthrough_vars() -> Vec<String> {
    llm_passthrough()
        .into_iter()
        .chain(UNCONDITIONAL_PASSTHROUGH.iter().copied())
        .map(String::from)
        .collect()
}

/// Whether any variable that could authenticate Claude against Anthropic is
/// present. Used only to decide whether to print the missing-credential
/// warning — never logged or otherwise surfaced itself.
pub fn any_anthropic_credential_set() -> bool {
    ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL", "CLAUDE_CODE_OAUTH_TOKEN"]
        .iter()
        .any(|k| is_set_and_non_empty(k))
}

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
    use super::{any_anthropic_credential_set, compose, parse_e_flag, passthrough_vars};
    use crate::test_env_lock::EnvVarGuard;

    /// Clears the three vars the conditional branches on, so each test below
    /// starts from a known "nothing set" baseline regardless of run order or
    /// the outer process's real environment.
    fn clear_llm_vars() -> (EnvVarGuard, EnvVarGuard, EnvVarGuard) {
        (
            EnvVarGuard::remove("CLAUDE_CODE_OAUTH_TOKEN"),
            EnvVarGuard::remove("ANTHROPIC_BASE_URL"),
            EnvVarGuard::remove("ANTHROPIC_API_KEY"),
        )
    }

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
        let _lock = crate::test_env_lock::lock();
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
        let _lock = crate::test_env_lock::lock();
        unsafe { std::env::remove_var("CBOX_T_ABSENT") };
        let out = compose(&[], &["CBOX_T_ABSENT".to_string()], &[]).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn bare_e_flag_forwards_from_the_host() {
        let _lock = crate::test_env_lock::lock();
        unsafe { std::env::set_var("CBOX_T_BARE", "v") };
        let out = compose(&["CBOX_T_BARE".to_string()], &[], &[]).unwrap();
        assert_eq!(out, vec![("CBOX_T_BARE".to_string(), "v".to_string())]);
    }

    #[test]
    fn a_secret_source_must_not_also_be_passed_through() {
        let _lock = crate::test_env_lock::lock();
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

    #[test]
    fn oauth_branch_forwards_the_oauth_token_and_base_url_only() {
        let _lock = crate::test_env_lock::lock();
        let _cleared = clear_llm_vars();
        let _oauth = EnvVarGuard::set("CLAUDE_CODE_OAUTH_TOKEN", "oauth-tok");
        let _base = EnvVarGuard::set("ANTHROPIC_BASE_URL", "http://gw");

        let vars = passthrough_vars();
        assert!(vars.contains(&"CLAUDE_CODE_OAUTH_TOKEN".to_string()));
        assert!(vars.contains(&"ANTHROPIC_BASE_URL".to_string()));
        assert!(!vars.contains(&"ANTHROPIC_API_KEY".to_string()));
        assert!(!vars.contains(&"ANTHROPIC_AUTH_TOKEN".to_string()));
    }

    /// The middle branch is the one the whole conditional exists to protect:
    /// in keyed-gateway mode (ANTHROPIC_BASE_URL set, no OAuth token) the real
    /// API key must stay host-side for the gateway to inject — a flat union
    /// that always forwarded ANTHROPIC_API_KEY would destroy that property
    /// silently, with no error and no visible symptom until someone inspected
    /// the box's environment.
    #[test]
    fn base_url_branch_withholds_the_raw_api_key() {
        let _lock = crate::test_env_lock::lock();
        let _cleared = clear_llm_vars();
        let _base = EnvVarGuard::set("ANTHROPIC_BASE_URL", "http://gw");

        let vars = passthrough_vars();
        assert!(vars.contains(&"ANTHROPIC_AUTH_TOKEN".to_string()));
        assert!(vars.contains(&"ANTHROPIC_BASE_URL".to_string()));
        assert!(
            !vars.contains(&"ANTHROPIC_API_KEY".to_string()),
            "keyed-gateway mode must not forward the real API key: {vars:?}"
        );
        assert!(!vars.contains(&"CLAUDE_CODE_OAUTH_TOKEN".to_string()));
    }

    #[test]
    fn direct_branch_forwards_the_api_key_when_neither_gateway_var_is_set() {
        let _lock = crate::test_env_lock::lock();
        let _cleared = clear_llm_vars();

        let vars = passthrough_vars();
        assert!(vars.contains(&"ANTHROPIC_API_KEY".to_string()));
        assert!(vars.contains(&"ANTHROPIC_AUTH_TOKEN".to_string()));
        assert!(!vars.contains(&"ANTHROPIC_BASE_URL".to_string()));
        assert!(!vars.contains(&"CLAUDE_CODE_OAUTH_TOKEN".to_string()));
    }

    /// The headline invariant this project exists to provide -- the token
    /// must reach the box as a secret placeholder, never as a plain
    /// passthrough value -- was previously guarded only by a doc comment on
    /// `UNCONDITIONAL_PASSTHROUGH`. Assert it directly, across all three
    /// mutually exclusive LLM-auth branches `llm_passthrough` can select.
    #[test]
    fn github_tokens_never_appear_in_passthrough_vars_in_any_branch() {
        let _lock = crate::test_env_lock::lock();
        let _cleared = clear_llm_vars();

        fn assert_no_github_tokens(vars: &[String]) {
            assert!(!vars.contains(&"GH_TOKEN".to_string()), "{vars:?}");
            assert!(!vars.contains(&"GITHUB_TOKEN".to_string()), "{vars:?}");
        }

        // Direct branch: neither ANTHROPIC_BASE_URL nor the OAuth token set.
        assert_no_github_tokens(&passthrough_vars());

        // base_url branch.
        let _base = EnvVarGuard::set("ANTHROPIC_BASE_URL", "http://gw");
        assert_no_github_tokens(&passthrough_vars());

        // oauth branch: takes priority over the base_url branch above.
        let _oauth = EnvVarGuard::set("CLAUDE_CODE_OAUTH_TOKEN", "oauth-tok");
        assert_no_github_tokens(&passthrough_vars());
    }

    #[test]
    fn unconditional_vars_are_present_in_every_branch() {
        let _lock = crate::test_env_lock::lock();
        let _cleared = clear_llm_vars();
        let vars = passthrough_vars();
        assert!(vars.contains(&"ANTHROPIC_MODEL".to_string()));
        assert!(vars.contains(&"GIT_AUTHOR_NAME".to_string()));
    }

    #[test]
    fn no_credential_set_is_detected_for_the_warning() {
        let _lock = crate::test_env_lock::lock();
        let _cleared = clear_llm_vars();
        let _auth = EnvVarGuard::remove("ANTHROPIC_AUTH_TOKEN");
        assert!(!any_anthropic_credential_set());

        let _key = EnvVarGuard::set("ANTHROPIC_API_KEY", "sk-something");
        assert!(any_anthropic_credential_set());
    }
}
