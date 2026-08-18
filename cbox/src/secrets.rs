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

/// The GitHub preset. Its `name` is a marker: `build` expands it into the two
/// secrets the two protocols need.
pub fn github_spec() -> SecretSpec {
    let env_var = if std::env::var("GH_TOKEN").is_ok() { "GH_TOKEN" } else { "GITHUB_TOKEN" };
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
        let value = std::env::var(&spec.env_var)
            .ok()
            .filter(|v| !v.is_empty())
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
pub fn git_bootstrap_script() -> String {
    format!(
        "git config --global http.https://github.com/.extraHeader \
         'Authorization: Basic {}' && \
         git config --global credential.helper ''",
        placeholder("gh_basic")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
        unsafe { std::env::set_var("CBOX_T_TOKEN", "super-secret") };
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

    #[test]
    fn github_produces_two_secrets_scoped_to_different_hosts() {
        unsafe { std::env::set_var("GH_TOKEN", "ghp_example") };
        let built = build(&[github_spec()]).unwrap();
        assert_eq!(built.secrets.len(), 2);

        let gh = built.secrets.iter().find(|s| s.name == "gh").unwrap();
        assert_eq!(gh.hosts, vec!["api.github.com".to_string()]);
        assert_eq!(gh.value, "ghp_example");

        let basic = built.secrets.iter().find(|s| s.name == "gh_basic").unwrap();
        assert_eq!(basic.hosts, vec!["github.com".to_string()]);
    }

    /// The whole git fix rests on this, and it is the one value never
    /// eyeballed in any output.
    #[test]
    fn the_git_blob_decodes_to_what_github_expects() {
        unsafe { std::env::set_var("GH_TOKEN", "ghp_example") };
        let built = build(&[github_spec()]).unwrap();
        let basic = built.secrets.iter().find(|s| s.name == "gh_basic").unwrap();

        let decoded = String::from_utf8(BASE64.decode(&basic.value).unwrap()).unwrap();
        assert_eq!(decoded, "x-access-token:ghp_example");
        // And the placeholder must NOT survive encoding — that asymmetry is
        // precisely why the credential-helper path cannot work and this can.
        assert!(!basic.value.contains("x-access-token"));
    }

    #[test]
    fn the_bootstrap_blanks_the_credential_helper() {
        let script = git_bootstrap_script();
        assert!(script.contains("<BOXLITE_SECRET:gh_basic>"));
        assert!(script.contains("extraHeader"));
        // Without this, git falls back to the helper on a 401 and sends
        // base64'd garbage.
        assert!(script.contains("credential.helper"));
    }
}
