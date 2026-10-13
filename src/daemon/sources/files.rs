//! The state-file sources: the gpu-select and power-override files, the
//! idle-suspend request, the monitor-profile TOML directories, and
//! `platform_profile`. All of them were read every 2 s. inotify reports the
//! files and directories, sysfs_notify (POLLPRI) reports platform_profile,
//! and the reconciler's safety tick re-reads everything as the net under a
//! lost event. One task owns every `last_*` value, so a change is reported
//! exactly once however it was noticed (a duplicate PlatformProfileChanged
//! would adopt an external write twice).

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use inotify::{EventMask, EventStream, Inotify, WatchDescriptor, WatchMask, Watches};
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

use crate::daemon::event::Event;
use crate::paths;
use crate::sysio::profiles::profiles_source_fingerprint;
use crate::sysio::sysfs::read_first_word;

/// What a directory watch stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dir {
    /// `~/.config/hypr`: gpu-select, power-override, and the `profiles`
    /// child appearing.
    HyprConfig,
    /// `$XDG_RUNTIME_DIR`: the idle-suspend request file.
    Runtime,
    /// `~/.config/hypr/profiles`: user TOML profiles.
    UserProfiles,
    /// `/etc/monitor-profiles`: shared TOML profiles.
    SystemProfiles,
}

const DIR_MASK: WatchMask = WatchMask::CREATE
    .union(WatchMask::DELETE)
    .union(WatchMask::CLOSE_WRITE)
    .union(WatchMask::MOVED_TO)
    .union(WatchMask::MOVED_FROM);

const GPU_OVERRIDE_NAME: &str = "gpu-select";
const POWER_OVERRIDE_NAME: &str = "power-override";
const SUSPEND_REQUEST_NAME: &str = "hyprstate-suspend-request";
const PROFILES_DIR_NAME: &str = "profiles";

/// The last value reported for each input. Reads here are the same ones
/// the old poller made; only the trigger changed.
struct Last {
    platform: Option<String>,
    gpu: Option<String>,
    power: Option<String>,
    suspend: bool,
    profiles: u64,
}

impl Last {
    fn read_now() -> Self {
        Last {
            platform: read_first_word(paths::platform_profile_path()),
            gpu: read_first_word(&paths::gpu_override_file()),
            power: read_first_word(&paths::power_override_file()),
            // Existence, not content: every other reader keys on the file
            // existing, and a zero-byte file must not read as "standing" to
            // one and "withdrawn" to another.
            suspend: paths::suspend_request_standing(),
            profiles: profiles_source_fingerprint(),
        }
    }
}

/// Which inputs an inotify event (or the tick) says to re-read.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Check {
    gpu: bool,
    power: bool,
    suspend: bool,
    profiles: bool,
    platform: bool,
    /// Re-add any directory watch that was missing (a dir created later).
    rewatch: bool,
}

impl Check {
    const ALL: Check = Check {
        gpu: true,
        power: true,
        suspend: true,
        profiles: true,
        platform: true,
        rewatch: true,
    };
}

/// Pure routing of one inotify event to the inputs it can have moved.
/// Self-echo is filtered here: the daemon itself renders `*.lua` into the
/// user profiles dir and writes gpu-profile and edp-off beside the
/// override files, and none of those is an input.
fn route(dir: Dir, mask: EventMask, name: Option<&str>) -> Check {
    let mut check = Check::default();
    if mask.contains(EventMask::Q_OVERFLOW) {
        return Check::ALL;
    }
    if mask.contains(EventMask::IGNORED) {
        // The watched directory itself went away; its inputs read as absent
        // now and the watch must be re-established if it comes back.
        return Check {
            profiles: matches!(dir, Dir::UserProfiles | Dir::SystemProfiles),
            rewatch: true,
            ..Check::default()
        };
    }
    let Some(name) = name else {
        return check;
    };
    match dir {
        Dir::HyprConfig => match name {
            GPU_OVERRIDE_NAME => check.gpu = true,
            POWER_OVERRIDE_NAME => check.power = true,
            PROFILES_DIR_NAME if mask.intersects(EventMask::CREATE | EventMask::MOVED_TO) => {
                check.profiles = true;
                check.rewatch = true;
            }
            _ => {}
        },
        Dir::Runtime => {
            if name == SUSPEND_REQUEST_NAME {
                check.suspend = true;
            }
        }
        Dir::UserProfiles | Dir::SystemProfiles => {
            if Path::new(name).extension().and_then(|e| e.to_str()) == Some("toml") {
                check.profiles = true;
            }
        }
    }
    check
}

struct DirWatches {
    wds: HashMap<WatchDescriptor, Dir>,
    runtime_dir: Option<PathBuf>,
}

impl DirWatches {
    fn targets(&self) -> Vec<(Dir, PathBuf)> {
        let mut out = Vec::new();
        if let Some(parent) = paths::power_override_file().parent() {
            out.push((Dir::HyprConfig, parent.to_path_buf()));
        }
        if let Some(rt) = &self.runtime_dir {
            out.push((Dir::Runtime, rt.clone()));
        }
        out.push((Dir::UserProfiles, paths::profiles_dir()));
        out.push((
            Dir::SystemProfiles,
            paths::system_profiles_dir().to_path_buf(),
        ));
        out
    }

    /// Add every directory watch that is not yet held. A missing directory
    /// is not an error (no shared profiles, no user profiles yet); it is
    /// retried on the tick and when its parent reports it created.
    fn ensure(&mut self, watches: &mut Watches) {
        for (dir, path) in self.targets() {
            if self.wds.values().any(|d| *d == dir) {
                continue;
            }
            match watches.add(&path, DIR_MASK) {
                Ok(wd) => {
                    debug!("watching {} ({dir:?})", path.display());
                    self.wds.insert(wd, dir);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    debug!("not watching {} ({dir:?}): absent", path.display());
                }
                Err(e) => warn!("inotify watch on {} failed: {e}", path.display()),
            }
        }
    }
}

/// `platform_profile` under sysfs_notify: the kernel raises POLLPRI on the
/// open attribute when the value changes (acpi platform_profile_store
/// calls sysfs_notify), and nothing else; inotify does not see sysfs.
struct PlatformWatch {
    fd: AsyncFd<File>,
}

impl PlatformWatch {
    fn open() -> Option<Self> {
        let path = paths::platform_profile_path();
        let file = File::open(path).ok()?;
        match AsyncFd::with_interest(file, Interest::PRIORITY) {
            Ok(fd) => Some(PlatformWatch { fd }),
            Err(e) => {
                warn!("platform_profile cannot be watched ({e}); tick only");
                None
            }
        }
    }

    /// Current first word, read through the watched descriptor so the
    /// kernel's change counter for this open file is consumed.
    fn read(&self) -> Option<String> {
        let mut file = self.fd.get_ref();
        file.seek(std::io::SeekFrom::Start(0)).ok()?;
        let mut text = String::new();
        file.read_to_string(&mut text).ok()?;
        text.split_whitespace().next().map(str::to_string)
    }

    /// Whether the attribute changed again since the last read (a notify
    /// that landed between the read and the readiness clear).
    fn changed_again(&self) -> bool {
        let mut pfd = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLPRI,
            revents: 0,
        };
        // SAFETY: pfd is a valid, initialised pollfd and nfds is 1.
        let n = unsafe { libc::poll(&mut pfd, 1, 0) };
        n > 0 && (pfd.revents & libc::POLLPRI) != 0
    }
}

/// Wait for POLLPRI on platform_profile, or for ever when there is none.
async fn platform_ready(watch: Option<&PlatformWatch>) -> std::io::Result<()> {
    match watch {
        Some(w) => {
            let mut guard = w.fd.ready(Interest::PRIORITY).await?;
            guard.clear_ready();
            Ok(())
        }
        None => std::future::pending().await,
    }
}

pub async fn file_state_task(tx: mpsc::Sender<Event>, mut tick: watch::Receiver<u64>) {
    let mut last = Last::read_now();
    let platform = PlatformWatch::open();
    if platform.is_none() && paths::platform_profile_path().exists() {
        warn!(
            "platform_profile present but not watchable; changes are seen on the safety tick only"
        );
    }

    let mut dirs = DirWatches {
        wds: HashMap::new(),
        runtime_dir: paths::suspend_request_file().and_then(|p| p.parent().map(Path::to_path_buf)),
    };
    let inotify = match Inotify::init() {
        Ok(i) => Some(i),
        Err(e) => {
            warn!("inotify unavailable ({e}); state files are re-read on the safety tick only");
            None
        }
    };
    let mut stream: Option<EventStream<[u8; 4096]>> = None;
    let mut watches: Option<Watches> = None;
    if let Some(inotify) = inotify {
        let mut w = inotify.watches();
        dirs.ensure(&mut w);
        match inotify.into_event_stream([0u8; 4096]) {
            Ok(s) => {
                watches = Some(w);
                stream = Some(s);
            }
            Err(e) => warn!(
                "inotify stream failed ({e}); state files are re-read on the safety tick only"
            ),
        }
    }
    info!(
        "state files: inotify on {} dirs, platform_profile {}",
        dirs.wds.len(),
        if platform.is_some() {
            "via sysfs notify"
        } else {
            "absent"
        }
    );

    loop {
        let check = tokio::select! {
            ev = next_inotify(stream.as_mut()) => match ev {
                Some(ev) => {
                    let dir = dirs.wds.get(&ev.wd).copied();
                    if ev.mask.contains(EventMask::IGNORED) {
                        dirs.wds.remove(&ev.wd);
                    }
                    let name = ev.name.as_deref().and_then(|n| n.to_str());
                    match dir {
                        Some(dir) => route(dir, ev.mask, name),
                        None => Check::default(),
                    }
                }
                None => {
                    warn!("inotify stream ended; state files are re-read on the safety tick only");
                    stream = None;
                    Check::default()
                }
            },
            res = platform_ready(platform.as_ref()) => {
                if let Err(e) = res {
                    warn!("platform_profile readiness failed: {e}");
                }
                Check { platform: true, ..Check::default() }
            }
            changed = tick.changed() => {
                if changed.is_err() {
                    return;
                }
                Check::ALL
            }
        };

        if check.rewatch
            && let Some(w) = watches.as_mut()
        {
            dirs.ensure(w);
        }
        if check.platform && !send_platform(&tx, &mut last, platform.as_ref()).await {
            return;
        }
        if check.gpu {
            let cur = read_first_word(&paths::gpu_override_file());
            if cur != last.gpu {
                last.gpu = cur.clone();
                if tx.send(Event::GpuOverrideChanged(cur)).await.is_err() {
                    return;
                }
            }
        }
        if check.power {
            let cur = read_first_word(&paths::power_override_file());
            if cur != last.power {
                last.power = cur.clone();
                if tx.send(Event::PowerOverrideChanged(cur)).await.is_err() {
                    return;
                }
            }
        }
        if check.suspend {
            let cur = paths::suspend_request_standing();
            if cur != last.suspend {
                last.suspend = cur;
                let payload = cur.then(|| "idle".to_string());
                if tx
                    .send(Event::SuspendRequestChanged(payload))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
        if check.profiles {
            let cur = profiles_source_fingerprint();
            if cur != last.profiles {
                last.profiles = cur;
                if tx.send(Event::ProfilesChanged).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// Next inotify event, or pending for ever once the stream is gone.
async fn next_inotify(
    stream: Option<&mut EventStream<[u8; 4096]>>,
) -> Option<inotify::Event<std::ffi::OsString>> {
    match stream {
        Some(s) => loop {
            match s.next().await {
                Some(Ok(ev)) => return Some(ev),
                Some(Err(e)) => warn!("inotify read error: {e}"),
                None => return None,
            }
        },
        None => std::future::pending().await,
    }
}

/// Read platform_profile (through the watched descriptor when there is
/// one) and report a change. Loops while the kernel says it moved again
/// under the read. Returns false when the dispatcher is gone.
async fn send_platform(
    tx: &mpsc::Sender<Event>,
    last: &mut Last,
    watch: Option<&PlatformWatch>,
) -> bool {
    loop {
        let cur = match watch {
            Some(w) => w.read(),
            None => read_first_word(paths::platform_profile_path()),
        };
        if cur != last.platform {
            last.platform = cur.clone();
            if tx.send(Event::PlatformProfileChanged(cur)).await.is_err() {
                return false;
            }
        }
        if !watch.is_some_and(PlatformWatch::changed_again) {
            return true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Check, Dir, route};
    use inotify::EventMask;

    #[test]
    fn override_files_route_by_exact_name() {
        let c = route(
            Dir::HyprConfig,
            EventMask::CLOSE_WRITE,
            Some("power-override"),
        );
        assert_eq!(
            c,
            Check {
                power: true,
                ..Check::default()
            }
        );
        let c = route(Dir::HyprConfig, EventMask::DELETE, Some("gpu-select"));
        assert_eq!(
            c,
            Check {
                gpu: true,
                ..Check::default()
            }
        );
    }

    /// The daemon's own writes beside the override files are not inputs.
    #[test]
    fn daemon_side_files_in_hypr_config_are_ignored() {
        for name in ["edp-off", "gpu-profile", "power.conf", "hyprland.lua"] {
            assert_eq!(
                route(Dir::HyprConfig, EventMask::CLOSE_WRITE, Some(name)),
                Check::default(),
                "{name}"
            );
        }
    }

    #[test]
    fn profiles_dir_appearing_rewatches_and_rereads() {
        let c = route(
            Dir::HyprConfig,
            EventMask::CREATE | EventMask::ISDIR,
            Some("profiles"),
        );
        assert!(c.profiles && c.rewatch);
    }

    #[test]
    fn suspend_request_routes_only_its_own_name() {
        let c = route(
            Dir::Runtime,
            EventMask::CREATE,
            Some("hyprstate-suspend-request"),
        );
        assert_eq!(
            c,
            Check {
                suspend: true,
                ..Check::default()
            }
        );
        assert_eq!(
            route(Dir::Runtime, EventMask::CREATE, Some("wayland-1.lock")),
            Check::default()
        );
    }

    /// Rendered `.lua` and the atomic `.tmp-<pid>` files the daemon writes
    /// into the profiles dir must not feed ProfilesChanged back to itself.
    #[test]
    fn profile_renders_do_not_echo() {
        for name in [
            "desk.lua",
            "desk.lua.tmp-4242",
            ".active.lua",
            ".active.lua.tmp",
        ] {
            assert_eq!(
                route(Dir::UserProfiles, EventMask::CLOSE_WRITE, Some(name)),
                Check::default(),
                "{name}"
            );
        }
        let c = route(Dir::SystemProfiles, EventMask::MOVED_TO, Some("desk.toml"));
        assert_eq!(
            c,
            Check {
                profiles: true,
                ..Check::default()
            }
        );
    }

    #[test]
    fn overflow_rereads_everything() {
        assert_eq!(route(Dir::Runtime, EventMask::Q_OVERFLOW, None), Check::ALL);
    }

    #[test]
    fn a_removed_profiles_dir_rereads_and_rewatches() {
        let c = route(Dir::UserProfiles, EventMask::IGNORED, None);
        assert!(c.profiles && c.rewatch && !c.power);
    }
}
