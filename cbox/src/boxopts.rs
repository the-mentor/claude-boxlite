//! Assemble the per-box create options.

use std::path::PathBuf;

use anyhow::{Result, bail};
use boxlite::{BoxOptions, RootfsSpec, Secret};
use boxlite::runtime::options::VolumeSpec;

/// Default disk size for a booted box, used when `--disk-size` is absent.
/// User-configurable via that flag (see `build` below); `--cpus`/`--memory`/
/// `-u`-style flags remain out of scope per the plan.
pub const DISK_SIZE_GB: u64 = 10;

pub struct UpFlags {
    pub image: String,
    pub cwd_mount: bool,
    pub volumes: Vec<String>,
    pub cmd: Vec<String>,
    pub invocation_dir: PathBuf,
    pub disk_size_gb: Option<u64>,
}

/// `hostPath:boxPath[:ro|rw]`
pub fn parse_volume(s: &str) -> Result<VolumeSpec> {
    let parts: Vec<&str> = s.split(':').collect();
    match parts.as_slice() {
        [host, guest] => {
            if host.is_empty() {
                bail!("-v: host path cannot be empty in {s:?}");
            }
            if guest.is_empty() {
                bail!("-v: guest path cannot be empty in {s:?}");
            }
            Ok(VolumeSpec {
                host_path: (*host).to_string(),
                guest_path: (*guest).to_string(),
                read_only: false,
            })
        }
        [host, guest, opt] => {
            if host.is_empty() {
                bail!("-v: host path cannot be empty in {s:?}");
            }
            if guest.is_empty() {
                bail!("-v: guest path cannot be empty in {s:?}");
            }
            let read_only = match *opt {
                "ro" => true,
                "rw" => false,
                _ => bail!("-v: unrecognized mount option {opt:?} in {s:?}; use ro|rw"),
            };
            Ok(VolumeSpec {
                host_path: (*host).to_string(),
                guest_path: (*guest).to_string(),
                read_only,
            })
        }
        _ => bail!("-v needs hostPath:boxPath[:ro|rw], got {s:?}"),
    }
}

pub fn build(
    flags: &UpFlags,
    secrets: Vec<Secret>,
    env: Vec<(String, String)>,
) -> Result<BoxOptions> {
    let mut volumes = Vec::new();
    if flags.cwd_mount {
        volumes.push(VolumeSpec {
            host_path: flags.invocation_dir.to_string_lossy().into_owned(),
            guest_path: "/workspace".to_string(),
            read_only: false,
        });
    }
    for v in &flags.volumes {
        volumes.push(parse_volume(v)?);
    }

    // 0 is rejected here rather than passed through: BoxOptions silently
    // ignores any value smaller than the base image, so `--disk-size 0`
    // would look accepted but do nothing — worth a clear error instead of a
    // quiet no-op. No upper bound: an absurdly large value is the user's
    // problem (and boxlite/the host will fail loudly enough on it).
    if flags.disk_size_gb == Some(0) {
        bail!("--disk-size must be greater than 0");
    }

    Ok(BoxOptions {
        rootfs: RootfsSpec::Image(flags.image.clone()),
        env,
        secrets,
        volumes,
        disk_size_gb: Some(flags.disk_size_gb.unwrap_or(DISK_SIZE_GB)),
        working_dir: Some("/workspace".to_string()),
        cmd: Some(flags.cmd.clone()),
        // Mandatory. The SDK default (false) stops the box when the creating
        // runtime drops, so exiting `cbox up` would destroy it.
        detach: true,
        // Must be false alongside detach: BoxOptions::sanitize() in the SDK
        // rejects auto_remove=true with detach=true outright ("Detached boxes
        // should use auto_remove=false for manual lifecycle control"). A
        // dropped runtime no longer stops the box, so removal happens only on
        // an explicit `cbox down`.
        auto_remove: false,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags() -> UpFlags {
        UpFlags {
            image: "claude-boxlite-custom".into(),
            cwd_mount: false,
            volumes: vec![],
            cmd: vec!["claude".into()],
            invocation_dir: PathBuf::from("/tmp/project"),
            disk_size_gb: None,
        }
    }

    #[test]
    fn parses_a_volume_and_its_readonly_form() {
        let v = parse_volume("/host:/box").unwrap();
        assert_eq!(v.host_path, "/host");
        assert_eq!(v.guest_path, "/box");
        assert!(!v.read_only);

        let v = parse_volume("/host:/box:ro").unwrap();
        assert!(v.read_only);
    }

    #[test]
    fn a_volume_without_a_colon_is_rejected() {
        assert!(parse_volume("/host").is_err());
    }

    #[test]
    fn a_volume_with_four_parts_is_rejected() {
        let err = parse_volume("/host:/box:ro:extra").unwrap_err().to_string();
        assert!(err.contains("hostPath:boxPath[:ro|rw]"), "error should hint at format: {err}");
    }

    #[test]
    fn an_empty_host_path_is_rejected() {
        let err = parse_volume(":/box").unwrap_err().to_string();
        assert!(err.contains("host path cannot be empty"), "error should name the problem: {err}");
    }

    #[test]
    fn an_empty_guest_path_is_rejected() {
        let err = parse_volume("/host:").unwrap_err().to_string();
        assert!(err.contains("guest path cannot be empty"), "error should name the problem: {err}");
    }

    #[test]
    fn an_unrecognized_mount_option_is_rejected() {
        // :RO is a plausible typo that would silently produce a writable mount
        // under the old implementation. It must be rejected.
        let err = parse_volume("/host:/box:RO").unwrap_err().to_string();
        assert!(err.contains("unrecognized mount option"), "error should name the problem: {err}");
        assert!(err.contains("ro|rw"), "error should hint at valid options: {err}");
    }

    #[test]
    fn rw_option_explicitly_sets_writable() {
        let v = parse_volume("/host:/box:rw").unwrap();
        assert!(!v.read_only, ":rw should produce a writable mount");
    }

    #[test]
    fn detach_is_always_true() {
        // The SDK default is false, which would destroy the box when cbox up
        // exits. This is the single most important line in the file.
        let opts = build(&flags(), vec![], vec![]).unwrap();
        assert!(opts.detach, "detach must be true or exiting cbox up kills the box");
    }

    #[test]
    fn the_command_is_set_explicitly() {
        // The image sets no ENTRYPOINT/CMD, so it inherits node:26's `node`,
        // which exits immediately without a TTY and takes the box with it.
        let opts = build(&flags(), vec![], vec![]).unwrap();
        assert_eq!(opts.cmd, Some(vec!["claude".to_string()]));
        assert_eq!(opts.working_dir, Some("/workspace".to_string()));
    }

    #[test]
    fn cwd_mount_maps_the_invocation_directory_onto_workspace() {
        let mut f = flags();
        f.cwd_mount = true;
        let opts = build(&f, vec![], vec![]).unwrap();
        assert_eq!(opts.volumes.len(), 1);
        assert_eq!(opts.volumes[0].host_path, "/tmp/project");
        assert_eq!(opts.volumes[0].guest_path, "/workspace");
    }

    #[test]
    fn absent_disk_size_flag_yields_the_default() {
        let opts = build(&flags(), vec![], vec![]).unwrap();
        assert_eq!(opts.disk_size_gb, Some(DISK_SIZE_GB));
    }

    #[test]
    fn a_provided_disk_size_reaches_box_options() {
        let mut f = flags();
        f.disk_size_gb = Some(40);
        let opts = build(&f, vec![], vec![]).unwrap();
        assert_eq!(opts.disk_size_gb, Some(40));
    }

    #[test]
    fn a_zero_disk_size_is_rejected() {
        let mut f = flags();
        f.disk_size_gb = Some(0);
        let err = build(&f, vec![], vec![]).unwrap_err().to_string();
        assert!(err.contains("--disk-size"), "error should name the flag: {err}");
    }

    #[test]
    fn secrets_and_env_are_carried_through() {
        let s = Secret {
            name: "gh".into(),
            hosts: vec!["api.github.com".into()],
            placeholder: "<BOXLITE_SECRET:gh>".into(),
            value: "tok".into(),
        };
        let opts = build(&flags(), vec![s], vec![("A".into(), "1".into())]).unwrap();
        assert_eq!(opts.secrets.len(), 1);
        assert!(opts.env.contains(&("A".to_string(), "1".to_string())));
    }
}
