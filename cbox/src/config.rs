//! Registry configuration and on-disk layout.
//!
//! The SDK takes `image_registries` as constructed values rather than a file
//! path, so the same `registries.local.json` the justfile passes to `just
//! build` has to be parsed and mapped here. Format and location are unchanged
//! so `registry-login.py` keeps working untouched.

use std::path::{Path, PathBuf};

use boxlite::ImageRegistry;

/// Map this repo's `--config` JSON onto `ImageRegistry` values.
///
/// A missing or malformed file yields no registries rather than an error: the
/// box still boots against docker.io, and failing the whole command because a
/// per-machine config is absent would be worse than the default.
pub fn load_registries(path: &Path) -> Vec<ImageRegistry> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return vec![];
    };
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return vec![];
    };
    let Some(entries) = doc.get("image_registries").and_then(|v| v.as_array()) else {
        return vec![];
    };

    entries
        .iter()
        .filter_map(|entry| {
            let host = entry.get("host")?.as_str()?;
            let http = entry.get("transport").and_then(|t| t.as_str()) == Some("http");
            let mut reg = if http {
                ImageRegistry::http(host)
            } else {
                ImageRegistry::https(host)
            };
            reg = reg
                .with_skip_verify(flag(entry, "skip_verify"))
                .with_search(flag(entry, "search"));

            let auth = entry.get("auth");
            let user = auth.and_then(|a| a.get("username")).and_then(|v| v.as_str());
            let pass = auth.and_then(|a| a.get("password")).and_then(|v| v.as_str());
            if let (Some(u), Some(p)) = (user, pass) {
                reg = reg.with_basic_auth(u, p);
            }
            Some(reg)
        })
        .collect()
}

fn flag(entry: &serde_json::Value, key: &str) -> bool {
    entry.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

/// `--config`, then `$CBOX_REGISTRIES`, then `~/.config/cbox/registries.json`.
pub fn resolve_config_path(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p.to_path_buf());
    }
    if let Ok(p) = std::env::var("CBOX_REGISTRIES") {
        if !p.trim().is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/cbox/registries.json"))
}

/// Each box name gets its own BOXLITE_HOME, which is what lets two boxes run
/// at once — BoxLite locks the whole home directory, not the individual box.
pub fn box_home(name: &str) -> PathBuf {
    let root = std::env::var("BOXLITE_HOME").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        format!("{home}/.boxlite")
    });
    PathBuf::from(root).join("boxes").join(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, body: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join("registries.json");
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn parses_transport_and_flattened_auth() {
        let dir = std::env::temp_dir().join("cbox-test-registries-ok");
        let p = write(
            &dir,
            r#"{"image_registries":[
                 {"host":"localhost:5551","transport":"http","skip_verify":true,"search":true},
                 {"host":"docker.io","transport":"https","search":true},
                 {"host":"ecr.example.com","auth":{"username":"AWS","password":"pw"}}
               ]}"#,
        );
        assert_eq!(load_registries(&p).len(), 3);
    }

    #[test]
    fn missing_file_yields_no_registries_rather_than_an_error() {
        assert!(load_registries(Path::new("/nonexistent/registries.json")).is_empty());
    }

    #[test]
    fn malformed_file_yields_no_registries_rather_than_panicking() {
        let dir = std::env::temp_dir().join("cbox-test-registries-bad");
        let p = write(&dir, "this is not json");
        assert!(load_registries(&p).is_empty());
    }

    #[test]
    fn entry_without_a_host_is_skipped_not_fatal() {
        let dir = std::env::temp_dir().join("cbox-test-registries-partial");
        let p = write(
            &dir,
            r#"{"image_registries":[{"transport":"https"},{"host":"docker.io"}]}"#,
        );
        assert_eq!(load_registries(&p).len(), 1);
    }

    #[test]
    fn box_home_is_per_name_under_the_boxes_root() {
        let home = box_home("claude-boxlite");
        assert!(home.ends_with("boxes/claude-boxlite"));
    }

    #[test]
    fn explicit_config_path_wins() {
        let explicit = PathBuf::from("/tmp/explicit.json");
        assert_eq!(resolve_config_path(Some(&explicit)), Some(explicit));
    }
}
