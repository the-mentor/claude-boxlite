//! Assemble the per-box create options.

use std::path::PathBuf;

use anyhow::{Result, bail};
use boxlite::{BoxOptions, RootfsSpec, Secret};
use boxlite::runtime::options::VolumeSpec;

/// Default disk size for a booted box, used when `--disk-size` is absent.
/// User-configurable via that flag (see `build` below); `--cpus`/`--memory`/
/// `-u`-style flags remain out of scope per the plan.
pub const DISK_SIZE_GB: u64 = 10;

/// The box's init process (PID 1 inside the container) -- never the user's
/// command.
///
/// `BoxOptions` has no field to request a TTY at all: a pty is allocated
/// only per-`Exec` RPC (`attach::attach` is what calls `.tty(true)`, on a
/// *separate* exec started after the box is up). So the init process can
/// never be the interactive one, no matter what the user asked to run. If
/// it were the user's command (e.g. `claude`), it would run with no TTY,
/// exit immediately, and — because it's PID 1 — its exit tears down the
/// whole container PID namespace, killing every other exec in it. That
/// includes the real, TTY-backed attach exec started a few milliseconds
/// later. Two launches of the same command, the first silently killing the
/// second, is the exact bug this constant exists to prevent.
///
/// `sleep infinity` is a real binary in this repo's image, checked rather
/// than assumed: `docker run --rm claude-boxlite-custom sh -c 'command -v
/// sleep && sleep --version'` reports `/usr/bin/sleep`, GNU coreutils 9.7,
/// against the actual image this repo builds
/// (`custom/Dockerfile` FROM `claude-boxlite-base` FROM `node:26-trixie-slim`).
/// coreutils is also an "essential" Debian package present in every
/// `-slim` variant, so this isn't a fluke of this one image either, and
/// GNU coreutils has supported the `infinity` duration since well before
/// 9.7.
const KEEP_ALIVE_CMD: &[&str] = &["sleep", "infinity"];

pub struct UpFlags {
    pub image: String,
    pub cwd_mount: bool,
    pub volumes: Vec<String>,
    pub cmd: Vec<String>,
    pub invocation_dir: PathBuf,
    pub disk_size_gb: Option<u64>,
    /// Whether the box should outlive this process. Mirrors `boxlite run`'s
    /// `-d`/`--detach`; see `build` for the full reasoning.
    pub detach: bool,
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
        // Never `flags.cmd` -- see `KEEP_ALIVE_CMD`'s doc comment for why the
        // init process and the user's command must never be the same thing.
        // What the user asked to run is launched separately, over a
        // TTY-backed exec, by `attach::attach(&litebox, &flags.cmd)`.
        cmd: Some(KEEP_ALIVE_CMD.iter().map(|s| s.to_string()).collect()),
        // Matches `boxlite run -it` with no `-d`, which is what the pre-cbox
        // justfile actually passed (verified against commit 27f4a74, the
        // last version of the justfile's `up` recipe before cbox replaced
        // it: `boxlite --home "$home" run -it --name "$name" ...` -- no
        // `--detach` anywhere). So by default the box does not outlive this
        // process: closing the terminal lets boxlite's own watchdog stop the
        // VM, exactly like `just up` always worked. `--detach`/`-d` on `up`
        // flips this for anyone who wants the box to stay up so `cbox exec`
        // can reach it after this session ends.
        detach: flags.detach,
        // Always false, on both branches:
        //  - detach: true -> BoxOptions::sanitize() rejects auto_remove:
        //    true alongside it outright ("Detached boxes should use
        //    auto_remove=false for manual lifecycle control").
        //  - detach: false -> this is what preserves the box (rootfs disk
        //    and DB record) across the watchdog stopping it, so a later
        //    `cbox up` can reuse and restart it instead of hitting a name
        //    collision. `cbox down` remains the only thing that removes a
        //    box either way.
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
            detach: false,
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
    fn detach_follows_the_flag() {
        let mut f = flags();
        f.detach = false;
        assert!(!build(&f, vec![], vec![]).unwrap().detach);

        f.detach = true;
        assert!(build(&f, vec![], vec![]).unwrap().detach);
    }

    #[test]
    fn auto_remove_is_always_false_regardless_of_detach() {
        // detach: true -> the SDK's own sanitize() rejects auto_remove: true
        // alongside it. detach: false -> false is what preserves the box
        // across the watchdog stopping it, so a later `cbox up` can reuse
        // it. Either way this must never be true.
        for detach in [false, true] {
            let mut f = flags();
            f.detach = detach;
            let opts = build(&f, vec![], vec![]).unwrap();
            assert!(!opts.auto_remove, "auto_remove must be false (detach={detach})");
        }
    }

    #[test]
    fn the_init_command_is_the_keep_alive_process_never_the_users_command() {
        // This *is* the bug: BoxOptions has no TTY field, so if the init
        // process were the user's command it would run with no TTY, exit
        // immediately, and tear the whole PID namespace down with it --
        // taking the real, TTY-backed attach exec down too. The init
        // command must be a fixed keep-alive, decoupled from whatever the
        // user asked to run.
        let f = flags();
        assert_eq!(f.cmd, vec!["claude".to_string()], "sanity: flags() carries a user command");

        let opts = build(&f, vec![], vec![]).unwrap();
        assert_eq!(opts.cmd, Some(vec!["sleep".to_string(), "infinity".to_string()]));
        assert_ne!(opts.cmd, Some(f.cmd), "the init command must never equal the user's command");
        assert_eq!(opts.working_dir, Some("/workspace".to_string()));
    }

    #[test]
    fn the_init_command_does_not_track_whatever_the_user_asks_to_run() {
        let mut f = flags();
        f.cmd = vec!["bash".into(), "-lc".into(), "echo hi".into()];
        let opts = build(&f, vec![], vec![]).unwrap();
        assert_eq!(
            opts.cmd,
            Some(vec!["sleep".to_string(), "infinity".to_string()]),
            "the init command is fixed no matter what -- cmd requests"
        );
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
