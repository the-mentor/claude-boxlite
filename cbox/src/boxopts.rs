//! Assemble the per-box create options.

use std::path::PathBuf;

use anyhow::{Result, bail};
use boxlite::{BoxOptions, RootfsSpec, Secret};
use boxlite::runtime::options::VolumeSpec;

/// Matches the justfile's `disk_size` default.
pub const DISK_SIZE_GB: u64 = 10;

pub struct UpFlags {
    pub image: String,
    pub cwd_mount: bool,
    pub volumes: Vec<String>,
    pub cmd: Vec<String>,
    pub invocation_dir: PathBuf,
}

/// `hostPath:boxPath[:ro]`
pub fn parse_volume(s: &str) -> Result<VolumeSpec> {
    let parts: Vec<&str> = s.split(':').collect();
    match parts.as_slice() {
        [host, guest] => Ok(VolumeSpec {
            host_path: (*host).to_string(),
            guest_path: (*guest).to_string(),
            read_only: false,
        }),
        [host, guest, opt] => Ok(VolumeSpec {
            host_path: (*host).to_string(),
            guest_path: (*guest).to_string(),
            read_only: *opt == "ro",
        }),
        _ => bail!("-v needs hostPath:boxPath[:ro], got {s:?}"),
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

    Ok(BoxOptions {
        rootfs: RootfsSpec::Image(flags.image.clone()),
        env,
        secrets,
        volumes,
        disk_size_gb: Some(DISK_SIZE_GB),
        working_dir: Some("/workspace".to_string()),
        cmd: Some(flags.cmd.clone()),
        // Mandatory. The SDK default (false) stops the box when the creating
        // runtime drops, so exiting `cbox up` would destroy it.
        detach: true,
        // Safe alongside detach: a dropped runtime no longer stops the box, so
        // removal happens only on an explicit `cbox down`.
        auto_remove: true,
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
