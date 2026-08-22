//! Per-box sidecar metadata.
//!
//! BoxLite has no field for "which directory created this box" — `BoxOptions`
//! has no `labels`, and `BoxInfo::labels` is dead in 0.9.7 (always empty).
//! cbox already owns the per-box home directory, so it tracks provenance
//! itself: a flat JSON file dropped next to the box's own data, read back by
//! `cbox list`.

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "cbox.json";

/// Kept small and obviously extensible — a flat object, more fields later.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sidecar {
    /// The directory `cbox up` was invoked from (the one that derived the
    /// box's name, and that `-c` mounts onto `/workspace`).
    pub origin: String,
    /// Hash of each secret's *value* at the time this box was created,
    /// keyed by secret name (e.g. "gh") -- never the value itself. Used only
    /// to detect credential drift when `cbox up` reuses this box: see
    /// `hash_secret_value` and `changed_secrets`. `#[serde(default)]` so
    /// boxes created before this field existed (or hand-edited sidecars)
    /// still parse, just with nothing to compare.
    #[serde(default)]
    pub secret_hashes: BTreeMap<String, u64>,
}

/// Hash a secret's value for drift detection. Never store or log the value
/// itself -- only this.
///
/// `DefaultHasher` (SipHash) rather than a cryptographic hash is the right
/// tool: this is a same-machine "did this change since the box was
/// created" check, not a security boundary (anyone who can read this
/// sidecar to recover the hash can just as easily read the environment
/// variable it came from), and it's already in `std`, so it costs nothing
/// new to depend on. Unlike `HashMap`'s `RandomState`, `DefaultHasher::new()`
/// uses fixed keys -- deterministic across calls and across process
/// restarts, which is what makes a hash written by one `cbox up` comparable
/// to one computed by a later one. Its exact algorithm isn't a stability
/// guarantee of std *across compiler/toolchain versions*, so a `cbox`
/// rebuilt against a different std could in principle flag an unchanged
/// secret as "changed" -- but the failure mode of that is a harmless
/// unnecessary "run -f" prompt, never a missed warning, so it doesn't
/// undermine the property this exists for.
pub fn hash_secret_value(value: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

/// Names of secrets present in both `old` and `current` whose hash differs
/// -- i.e. the box's baked-in credential no longer matches what's in the
/// environment now (a rotated token being the case that matters). A secret
/// present in only one side is not reported: that's a secret being added or
/// dropped, not one going stale, and it isn't distinguishable here from a
/// box that simply never had that secret.
pub fn changed_secrets(old: &BTreeMap<String, u64>, current: &BTreeMap<String, u64>) -> Vec<String> {
    let mut changed: Vec<String> = old
        .iter()
        .filter_map(|(name, hash)| match current.get(name) {
            Some(current_hash) if current_hash != hash => Some(name.clone()),
            _ => None,
        })
        .collect();
    changed.sort();
    changed
}

/// Write (or overwrite) the sidecar for a box home.
///
/// Failure here is never fatal to `cbox up` — the caller is expected to warn
/// and continue, the same way the GitHub credential bootstrap does.
pub fn write(home: &Path, origin: &Path, secret_hashes: &BTreeMap<String, u64>) -> Result<()> {
    let sidecar = Sidecar {
        origin: origin.to_string_lossy().into_owned(),
        secret_hashes: secret_hashes.clone(),
    };
    let body = serde_json::to_string_pretty(&sidecar).context("failed to encode box metadata")?;
    let path = home.join(FILE_NAME);
    // Box homes are reused across `--force` recreates under the same derived
    // name. Unlink any previous occupant's sidecar *before* writing the new
    // one, so that if the write below fails partway, `list` renders this
    // box's origin as blank rather than silently showing the *previous*
    // occupant's origin against the new box. A missing file is not an error
    // here — there's nothing to remove on a box's first write.
    let _ = std::fs::remove_file(&path);
    std::fs::write(&path, body).with_context(|| format!("failed to write {}", path.display()))
}

/// Read the sidecar back.
///
/// Missing or malformed is not an error — boxes created before this existed
/// have no sidecar at all, and a hand-edited or truncated file is not
/// distinguishable from "no data" here. Either way `cbox list` renders an
/// empty origin cell rather than failing.
pub fn read(home: &Path) -> Option<Sidecar> {
    let raw = std::fs::read_to_string(home.join(FILE_NAME)).ok()?;
    serde_json::from_str(&raw).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("cbox-test-sidecar-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn write_then_read_round_trips_the_origin() {
        let home = tmp_home("roundtrip");
        write(&home, Path::new("/repo/checkout"), &BTreeMap::new()).expect("write should succeed");
        let sidecar = read(&home).expect("sidecar should parse");
        assert_eq!(sidecar.origin, "/repo/checkout");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn write_then_read_round_trips_secret_hashes() {
        let home = tmp_home("secret-hashes");
        let mut hashes = BTreeMap::new();
        hashes.insert("gh".to_string(), hash_secret_value("ghp_example"));
        hashes.insert("openai".to_string(), hash_secret_value("sk-example"));

        write(&home, Path::new("/repo"), &hashes).expect("write should succeed");
        let sidecar = read(&home).expect("sidecar should parse");
        assert_eq!(sidecar.secret_hashes, hashes);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn a_sidecar_written_before_secret_hashes_existed_reads_as_an_empty_map() {
        let home = tmp_home("pre-existing-sidecar");
        std::fs::write(home.join(FILE_NAME), r#"{"origin":"/repo"}"#).unwrap();
        let sidecar = read(&home).expect("sidecar should parse without the newer field");
        assert!(sidecar.secret_hashes.is_empty());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn hash_secret_value_is_deterministic_and_distinguishes_different_values() {
        assert_eq!(hash_secret_value("a-token"), hash_secret_value("a-token"));
        assert_ne!(hash_secret_value("a-token"), hash_secret_value("a-different-token"));
    }

    #[test]
    fn changed_secrets_reports_only_names_present_on_both_sides_with_a_different_hash() {
        let mut old = BTreeMap::new();
        old.insert("gh".to_string(), hash_secret_value("old-token"));
        old.insert("openai".to_string(), hash_secret_value("unchanged"));

        let mut current = BTreeMap::new();
        current.insert("gh".to_string(), hash_secret_value("rotated-token"));
        current.insert("openai".to_string(), hash_secret_value("unchanged"));
        // Present only in `current` -- a newly added secret, not a rotation
        // of anything this box was created with, so it must not be reported.
        current.insert("aws".to_string(), hash_secret_value("brand-new"));

        assert_eq!(changed_secrets(&old, &current), vec!["gh".to_string()]);
    }

    #[test]
    fn changed_secrets_ignores_a_secret_dropped_from_the_environment() {
        // Present only in `old` -- the environment no longer sets it. That's
        // not this box's problem to flag; it just means the box still has
        // whatever it was created with.
        let mut old = BTreeMap::new();
        old.insert("gh".to_string(), hash_secret_value("token"));
        let current = BTreeMap::new();

        assert!(changed_secrets(&old, &current).is_empty());
    }

    #[test]
    fn missing_sidecar_reads_as_none_not_an_error() {
        let home = tmp_home("missing");
        assert!(read(&home).is_none());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn malformed_sidecar_reads_as_none_rather_than_panicking() {
        let home = tmp_home("malformed");
        std::fs::write(home.join(FILE_NAME), "this is not json").unwrap();
        assert!(read(&home).is_none());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn empty_sidecar_file_reads_as_none() {
        let home = tmp_home("empty");
        std::fs::write(home.join(FILE_NAME), "").unwrap();
        assert!(read(&home).is_none());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn write_overwrites_a_previous_sidecar() {
        let home = tmp_home("overwrite");
        write(&home, Path::new("/first"), &BTreeMap::new()).unwrap();
        write(&home, Path::new("/second"), &BTreeMap::new()).unwrap();
        assert_eq!(read(&home).unwrap().origin, "/second");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn a_write_that_cannot_complete_leaves_no_stale_value_behind() {
        let home = tmp_home("write-fails");
        write(&home, Path::new("/first"), &BTreeMap::new()).unwrap();
        assert_eq!(read(&home).unwrap().origin, "/first");

        // Force the next write to fail outright: replace the sidecar path
        // with a directory, so neither the removal nor the write below can
        // touch it as a regular file.
        let path = home.join(FILE_NAME);
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();

        assert!(write(&home, Path::new("/second"), &BTreeMap::new()).is_err());

        // Whatever happened, `list` must never render this box as
        // "/first" again -- blank (None) is acceptable, the stale old
        // value is not.
        assert_ne!(read(&home).map(|s| s.origin), Some("/first".to_string()));

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    #[cfg(unix)]
    fn write_removes_a_stale_sidecar_rather_than_writing_through_it() {
        // A box home reused across a `--force` recreate could, in
        // principle, have anything left at `cbox.json` by a previous
        // occupant -- including something `fs::write`'s open-and-truncate
        // would follow rather than replace, like a symlink. Unlinking first
        // (this test's actual point) means the new sidecar always lands as
        // a plain file, and whatever the old path pointed at is untouched.
        let home = tmp_home("symlink-stale");
        let path = home.join(FILE_NAME);
        let elsewhere = home.join("elsewhere.json");
        std::fs::write(&elsewhere, r#"{"origin":"/stale-elsewhere"}"#).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &path).unwrap();

        write(&home, Path::new("/second"), &BTreeMap::new()).expect("write should succeed");

        let metadata = std::fs::symlink_metadata(&path).unwrap();
        assert!(
            !metadata.file_type().is_symlink(),
            "sidecar path should be a plain file after write, not a followed symlink"
        );
        assert_eq!(read(&home).unwrap().origin, "/second");
        let elsewhere_contents = std::fs::read_to_string(&elsewhere).unwrap();
        assert!(
            elsewhere_contents.contains("stale-elsewhere"),
            "the file the old symlink pointed at must be untouched: {elsewhere_contents:?}"
        );

        std::fs::remove_dir_all(&home).ok();
    }
}
