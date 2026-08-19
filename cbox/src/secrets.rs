//! Host-side credential substitution.
//!
//! GitHub needs two secrets because `gh` and `git` want the credential in
//! incompatible shapes. `gh` sends `Authorization: Bearer <placeholder>`, which
//! the proxy can match on the wire. `git` builds `Basic base64(user:token)`
//! itself, and base64 hides the placeholder from a literal string matcher —
//! measured against github.com, git-upload-pack returns 401 for Bearer and 200
//! for Basic, so Bearer is not an escape either. The fix is to move the base64
//! to the host: store the already-encoded credential as the value and put the
//! placeholder where the encoded blob belongs, via http.extraHeader, which git
//! passes through verbatim.

use anyhow::{Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use boxlite::Secret;

/// The Basic-auth username GitHub expects when the password is a PAT.
pub const GH_BASIC_USER: &str = "x-access-token";

/// Must match the Python binding's format, or the same box would present
/// different placeholders depending on which SDK launched it.
pub fn placeholder(name: &str) -> String {
    format!("<BOXLITE_SECRET:{name}>")
}

#[derive(Debug, Clone, PartialEq)]
pub struct SecretSpec {
    pub name: String,
    pub env_var: String,
    pub hosts: Vec<String>,
}

#[derive(Debug)]
pub struct Built {
    pub secrets: Vec<Secret>,
    pub env: Vec<(String, String)>,
    pub source_vars: Vec<String>,
}

/// `NAME=ENV_VAR@host[,host...]`. The flag names the variable; the value
/// never enters argv, where ps and shell history would capture it.
pub fn parse_secret_flag(s: &str) -> Result<SecretSpec> {
    let (lhs, hosts) = s
        .split_once('@')
        .ok_or_else(|| anyhow!("--secret needs @hosts: {s:?} (e.g. openai=OPENAI_API_KEY@api.openai.com)"))?;
    let (name, env_var) = lhs
        .split_once('=')
        .ok_or_else(|| anyhow!("--secret needs NAME=ENV_VAR: {s:?}"))?;

    if name.is_empty() || env_var.is_empty() {
        bail!("--secret needs a non-empty name and variable: {s:?}");
    }

    let hosts: Vec<String> = hosts
        .split(',')
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(String::from)
        .collect();
    if hosts.is_empty() {
        bail!("--secret {name} has no hosts. An unscoped secret is substituted on requests to any host.");
    }

    Ok(SecretSpec { name: name.to_string(), env_var: env_var.to_string(), hosts })
}

/// A variable counts as a usable source only if it is both set and non-empty.
/// `github_spec` and `build` must agree on this predicate: picking a variable
/// here that `build` would then reject as empty produces a misleading error
/// pointing at the wrong variable.
fn set_and_non_empty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Whether GitHub secret support should be requested at all. Built on the
/// same `set_and_non_empty` predicate as `github_spec`/`build`, so it cannot
/// drift from what those actually treat as usable: in particular,
/// `GH_TOKEN=""` (e.g. a blank line in a sourced `.env`) must not count as
/// present when `GITHUB_TOKEN` is also unset, or a caller that gates on this
/// and then calls `build` unconditionally would abort on a spurious "unset or
/// empty" error instead of silently skipping the secret, as having neither
/// variable set at all does.
pub fn has_github_token() -> bool {
    set_and_non_empty("GH_TOKEN").is_some() || set_and_non_empty("GITHUB_TOKEN").is_some()
}

/// The GitHub preset. Its `name` is a marker: `build` expands it into the two
/// secrets the two protocols need.
pub fn github_spec() -> SecretSpec {
    let env_var = if set_and_non_empty("GH_TOKEN").is_some() { "GH_TOKEN" } else { "GITHUB_TOKEN" };
    SecretSpec {
        name: "gh".into(),
        env_var: env_var.into(),
        hosts: vec!["api.github.com".into(), "github.com".into()],
    }
}

/// Read each spec's source variable and build the SDK secrets plus the
/// placeholder environment the guest receives.
pub fn build(specs: &[SecretSpec]) -> Result<Built> {
    let mut secrets = Vec::new();
    let mut env = Vec::new();
    let mut source_vars = Vec::new();

    for spec in specs {
        // Hosts are mandatory on every constructed secret — this is the
        // property the feature exists to provide, so it is enforced here
        // rather than trusted from the caller. The "gh" marker is exempt: it
        // hardcodes its own two host lists below instead of using spec.hosts.
        if spec.name != "gh" && spec.hosts.is_empty() {
            bail!(
                "--secret {} has no hosts. An unscoped secret is substituted on requests to any host.",
                spec.name
            );
        }

        let value = set_and_non_empty(&spec.env_var)
            .ok_or_else(|| anyhow!("--secret {} names {}, which is unset or empty", spec.name, spec.env_var))?;

        source_vars.push(spec.env_var.clone());

        if spec.name == "gh" {
            // Two secrets: gh sends Bearer to api.github.com; git sends Basic
            // to github.com and encodes it itself, so the encoding moves here.
            secrets.push(Secret {
                name: "gh".into(),
                hosts: vec!["api.github.com".into()],
                placeholder: placeholder("gh"),
                value: value.clone(),
            });
            secrets.push(Secret {
                name: "gh_basic".into(),
                hosts: vec!["github.com".into()],
                placeholder: placeholder("gh_basic"),
                value: BASE64.encode(format!("{GH_BASIC_USER}:{value}")),
            });
            env.push((spec.env_var.clone(), placeholder("gh")));
        } else {
            secrets.push(Secret {
                name: spec.name.clone(),
                hosts: spec.hosts.clone(),
                placeholder: placeholder(&spec.name),
                value,
            });
            env.push((spec.env_var.clone(), placeholder(&spec.name)));
        }
    }

    Ok(Built { secrets, env, source_vars })
}

/// Run inside the box after creation when GitHub secrets are present.
///
/// Blanking the helper is required, not tidiness: without it git falls back to
/// it on a 401 and sends base64'd garbage, which fails confusingly.
///
/// The blank must be scoped to `credential.https://github.com.helper`, not
/// the generic `credential.helper` — git resolves credential config by
/// urlmatch specificity, not by which file it came from, so a generic
/// `--global` blank does not override a URL-scoped entry from a lower-
/// precedence file (there is no such entry baked into the image anymore, but
/// a scoped blank is also what correctly overrides one if some other layer
/// ever adds it back). Measured with git 2.50.1 via
/// `git config --get-urlmatch credential https://github.com`.
pub fn git_bootstrap_script() -> String {
    format!(
        "git config --global http.https://github.com/.extraHeader \
         'Authorization: Basic {}' && \
         git config --global credential.https://github.com.helper ''",
        placeholder("gh_basic")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // Panic-safe env var mutation for tests lives in `test_env_lock`, shared
    // with every other module whose tests touch the environment. Binding the
    // guard to `_` still drops it immediately, so every call site below
    // binds it to a named local.
    use crate::test_env_lock::EnvVarGuard as EnvGuard;

    #[test]
    fn placeholder_matches_the_python_binding_format() {
        assert_eq!(placeholder("gh"), "<BOXLITE_SECRET:gh>");
        assert_eq!(placeholder("gh_basic"), "<BOXLITE_SECRET:gh_basic>");
    }

    #[test]
    fn parses_a_secret_flag_with_one_and_many_hosts() {
        let s = parse_secret_flag("openai=OPENAI_API_KEY@api.openai.com").unwrap();
        assert_eq!(s, SecretSpec {
            name: "openai".into(),
            env_var: "OPENAI_API_KEY".into(),
            hosts: vec!["api.openai.com".into()],
        });

        let s = parse_secret_flag("x=X@a.com,b.com").unwrap();
        assert_eq!(s.hosts, vec!["a.com".to_string(), "b.com".to_string()]);
    }

    #[test]
    fn a_secret_without_hosts_is_rejected() {
        // An unscoped secret is substituted on requests to ANY host, which
        // inverts the property the feature exists to provide.
        assert!(parse_secret_flag("openai=OPENAI_API_KEY").is_err());
        assert!(parse_secret_flag("openai=OPENAI_API_KEY@").is_err());
    }

    #[test]
    fn a_secret_without_an_env_var_is_rejected() {
        assert!(parse_secret_flag("openai@api.openai.com").is_err());
    }

    #[test]
    fn build_reports_a_missing_source_variable() {
        let _lock = crate::test_env_lock::lock();
        unsafe { std::env::remove_var("CBOX_T_MISSING") };
        let spec = SecretSpec {
            name: "x".into(),
            env_var: "CBOX_T_MISSING".into(),
            hosts: vec!["a.com".into()],
        };
        let err = build(&[spec]).unwrap_err().to_string();
        assert!(err.contains("CBOX_T_MISSING"), "names the variable: {err}");
    }

    #[test]
    fn build_sets_the_guest_variable_to_the_placeholder_not_the_value() {
        let _lock = crate::test_env_lock::lock();
        let _guard = EnvGuard::set("CBOX_T_TOKEN", "super-secret");
        let spec = SecretSpec {
            name: "mysecret".into(),
            env_var: "CBOX_T_TOKEN".into(),
            hosts: vec!["a.com".into()],
        };
        let built = build(&[spec]).unwrap();
        assert_eq!(
            built.env,
            vec![("CBOX_T_TOKEN".to_string(), "<BOXLITE_SECRET:mysecret>".to_string())]
        );
        assert_eq!(built.source_vars, vec!["CBOX_T_TOKEN".to_string()]);
        // The real value must never appear in what the guest receives.
        assert!(!built.env.iter().any(|(_, v)| v.contains("super-secret")));
    }

    /// `has_github_token` must agree with `github_spec`/`build` on what counts
    /// as usable, or a caller gating on it can still hit `build`'s "unset or
    /// empty" error. In particular `GH_TOKEN=""` with no `GITHUB_TOKEN` must
    /// read as absent, exactly like having neither variable set at all — this
    /// is the exact bug this function exists to prevent.
    #[test]
    fn has_github_token_treats_a_blank_gh_token_as_absent() {
        let _lock = crate::test_env_lock::lock();
        unsafe { std::env::remove_var("GH_TOKEN") };
        unsafe { std::env::remove_var("GITHUB_TOKEN") };
        assert!(!has_github_token(), "both unset");

        let _gh = EnvGuard::set("GH_TOKEN", "");
        assert!(!has_github_token(), "GH_TOKEN=\"\" with GITHUB_TOKEN unset");

        let _ghub = EnvGuard::set("GITHUB_TOKEN", "ghp_from_github_token");
        assert!(has_github_token(), "GH_TOKEN=\"\" with a valid GITHUB_TOKEN");
    }

    #[test]
    fn github_produces_two_secrets_scoped_to_different_hosts() {
        let _lock = crate::test_env_lock::lock();
        let _guard = EnvGuard::set("GH_TOKEN", "ghp_example");
        let built = build(&[github_spec()]).unwrap();
        assert_eq!(built.secrets.len(), 2);

        let gh = built.secrets.iter().find(|s| s.name == "gh").unwrap();
        assert_eq!(gh.hosts, vec!["api.github.com".to_string()]);
        assert_eq!(gh.value, "ghp_example");

        let basic = built.secrets.iter().find(|s| s.name == "gh_basic").unwrap();
        assert_eq!(basic.hosts, vec!["github.com".to_string()]);
    }

    /// `GH_TOKEN` set-but-empty must not shadow a perfectly usable
    /// `GITHUB_TOKEN` — `github_spec` and `build` must agree on what "usable"
    /// means, or this fails with a misleading error about the wrong variable.
    #[test]
    fn github_prefers_github_token_when_gh_token_is_set_but_empty() {
        let _lock = crate::test_env_lock::lock();
        let _gh = EnvGuard::set("GH_TOKEN", "");
        let _ghub = EnvGuard::set("GITHUB_TOKEN", "ghp_from_github_token");

        let spec = github_spec();
        assert_eq!(spec.env_var, "GITHUB_TOKEN");

        let built = build(&[spec]).unwrap();
        assert_eq!(built.source_vars, vec!["GITHUB_TOKEN".to_string()]);
        let gh = built.secrets.iter().find(|s| s.name == "gh").unwrap();
        assert_eq!(gh.value, "ghp_from_github_token");
    }

    /// The whole git fix rests on this, and it is the one value never
    /// eyeballed in any output.
    #[test]
    fn the_git_blob_decodes_to_what_github_expects() {
        let _lock = crate::test_env_lock::lock();
        let _guard = EnvGuard::set("GH_TOKEN", "ghp_example");
        let built = build(&[github_spec()]).unwrap();
        let basic = built.secrets.iter().find(|s| s.name == "gh_basic").unwrap();

        let decoded = String::from_utf8(BASE64.decode(&basic.value).unwrap()).unwrap();
        assert_eq!(decoded, "x-access-token:ghp_example");
        // And the placeholder must NOT survive encoding — that asymmetry is
        // precisely why the credential-helper path cannot work and this can.
        assert!(!basic.value.contains("x-access-token"));
    }

    /// `parse_secret_flag` already rejects an empty host list, but `build` is
    /// where `boxlite::Secret`s actually get constructed and handed across
    /// the SDK boundary — the "hosts are mandatory" invariant must not rest
    /// solely on caller discipline.
    #[test]
    fn build_rejects_a_hand_constructed_spec_with_no_hosts() {
        let spec = SecretSpec {
            name: "x".into(),
            env_var: "CBOX_T_NOHOSTS".into(),
            hosts: vec![],
        };
        let err = build(&[spec]).unwrap_err().to_string();
        assert!(err.contains("no hosts"), "names the problem: {err}");
    }

    #[test]
    fn the_bootstrap_blanks_the_credential_helper() {
        let script = git_bootstrap_script();
        assert!(script.contains("<BOXLITE_SECRET:gh_basic>"));
        assert!(script.contains("extraHeader"));
        // Without this, git falls back to the helper on a 401 and sends
        // base64'd garbage. It must be scoped to the same URL
        // (`credential.https://github.com.helper`), not the generic
        // `credential.helper` -- git resolves credential config by urlmatch
        // specificity, so a generic blank does not override a URL-scoped
        // entry from another config file. Measured live with
        // `git config --get-urlmatch credential https://github.com`.
        assert!(
            script.contains("credential.https://github.com.helper"),
            "the blank must be scoped to the same URL, not the generic key: {script}"
        );
    }
}
