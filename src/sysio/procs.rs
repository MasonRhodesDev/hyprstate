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
    use super::uid_from_status;

    #[test]
    fn reads_the_real_uid() {
        let status = "Name:\twaybar\nUmask:\t0022\nState:\tS (sleeping)\nUid:\t1000\t1000\t1000\t1000\nGid:\t1000\t1000\t1000\t1000\n";
        assert_eq!(uid_from_status(status), Some(1000));
        assert_eq!(uid_from_status("Name:\tx\n"), None);
    }
}
