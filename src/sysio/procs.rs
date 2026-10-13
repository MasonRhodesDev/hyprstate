//! /proc helpers: find the caller's own processes by name and signal them.

use std::fs;

/// Real uid from a `/proc/<pid>/status` body (`Uid:\treal\teffective...`).
pub fn uid_from_status(status: &str) -> Option<u32> {
    status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|uid| uid.parse().ok())
}

/// Send `signal` to every process named `comm` (exact `/proc/<pid>/comm`
/// match) that belongs to this uid, excluding ourselves. Returns how many
/// were signalled. Best effort: a process that exits mid-scan is skipped.
pub fn signal_own_processes(comm: &str, signal: libc::c_int) -> usize {
    // SAFETY: getuid and getpid have no preconditions.
    let (me_uid, me_pid) = unsafe { (libc::getuid(), libc::getpid()) };
    let Ok(rd) = fs::read_dir("/proc") else {
        return 0;
    };
    let mut hit = 0;
    for entry in rd.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|n| n.parse::<libc::pid_t>().ok()) else {
            continue;
        };
        if pid == me_pid {
            continue;
        }
        let path = entry.path();
        let Ok(proc_comm) = fs::read_to_string(path.join("comm")) else {
            continue;
        };
        if proc_comm.trim_end() != comm {
            continue;
        }
        let Ok(status) = fs::read_to_string(path.join("status")) else {
            continue;
        };
        if uid_from_status(&status) != Some(me_uid) {
            continue;
        }
        // SAFETY: kill with a pid we just read from /proc and a valid signal.
        if unsafe { libc::kill(pid, signal) } == 0 {
            hit += 1;
        }
    }
    hit
}

#[cfg(test)]
mod tests {
    use super::{signal_own_processes, uid_from_status};

    #[test]
    fn reads_the_real_uid() {
        let status = "Name:\twaybar\nUmask:\t0022\nState:\tS (sleeping)\nUid:\t1000\t1000\t1000\t1000\nGid:\t1000\t1000\t1000\t1000\n";
        assert_eq!(uid_from_status(status), Some(1000));
        assert_eq!(uid_from_status("Name:\tx\n"), None);
    }

    /// Exec `sleep` through a symlink so its comm is a name nothing else on
    /// the box carries (15 chars max, or comm truncates), then signal by
    /// that name: exactly one hit, and the child died of that signal.
    #[test]
    fn signals_exactly_the_processes_with_that_name() {
        use std::os::unix::process::ExitStatusExt;
        let name = "hs-sig-target";
        let dir = std::env::temp_dir().join(format!("hyprstate-procs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let link = dir.join(name);
        std::os::unix::fs::symlink("/bin/sleep", &link).unwrap();
        let mut child = std::process::Command::new(&link).arg("30").spawn().unwrap();
        // comm is set at exec; give the child a moment to get there.
        let comm = format!("/proc/{}/comm", child.id());
        for _ in 0..200 {
            if std::fs::read_to_string(&comm).is_ok_and(|c| c.trim_end() == name) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(signal_own_processes(name, libc::SIGTERM), 1);
        let status = child.wait().unwrap();
        assert_eq!(status.signal(), Some(libc::SIGTERM));
        assert_eq!(signal_own_processes(name, libc::SIGTERM), 0, "gone now");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
