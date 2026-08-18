//! Per-box sidecar metadata.
//!
//! BoxLite has no field for "which directory created this box" — `BoxOptions`
//! has no `labels`, and `BoxInfo::labels` is dead in 0.9.7 (always empty).
//! cbox already owns the per-box home directory, so it tracks provenance
//! itself: a flat JSON file dropped next to the box's own data, read back by
//! `cbox list`.

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
}

/// Write (or overwrite) the sidecar for a box home.
///
/// Failure here is never fatal to `cbox up` — the caller is expected to warn
/// and continue, the same way the GitHub credential bootstrap does.
pub fn write(home: &Path, origin: &Path) -> Result<()> {
    let sidecar = Sidecar { origin: origin.to_string_lossy().into_owned() };
    let body = serde_json::to_string_pretty(&sidecar).context("failed to encode box metadata")?;
    let path = home.join(FILE_NAME);
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
        write(&home, Path::new("/repo/checkout")).expect("write should succeed");
        let sidecar = read(&home).expect("sidecar should parse");
        assert_eq!(sidecar.origin, "/repo/checkout");
        std::fs::remove_dir_all(&home).ok();
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
        write(&home, Path::new("/first")).unwrap();
        write(&home, Path::new("/second")).unwrap();
        assert_eq!(read(&home).unwrap().origin, "/second");
        std::fs::remove_dir_all(&home).ok();
    }
}
