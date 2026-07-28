//! Emulator child-process bookkeeping: every spawned co-sim child (Renode,
//! Espressif QEMU) registers here so that
//!
//! 1. a normal teardown (`Drop`) kills the child's whole process tree, not
//!    just the direct child, and
//! 2. a fatal signal to the parent (SIGTERM / SIGINT / SIGHUP) still reaps
//!    every live emulator before the process dies, instead of orphaning them.
//!
//! Why this exists: killing a `hauksbee serve` with live co-sim sessions used
//! to leak its `qemu-system-*` children. Rust runs no `Drop` when the default
//! signal disposition terminates the process, so every SIGTERM/SIGINT of the
//! server orphaned the emulators; 23 accumulated `qemu-system-xtensa`
//! processes were found on one dev machine. The fix has two independent
//! layers: process-tree kills on `Drop` (group kill on unix, `taskkill /T` on
//! Windows), and [`install_signal_reaper`] for the paths where `Drop` never
//! runs.
//!
//! The registry is a fixed-capacity, lock-free table of PIDs because the
//! signal handler iterates it, and a signal handler must never take a lock the
//! interrupted thread might hold. Everything the handler touches is
//! async-signal-safe: atomic loads, `kill(2)`, `signal(2)`, `raise(2)`.

use std::sync::atomic::{AtomicU32, Ordering};

/// Far above the server's live-session cap. If it ever fills, extra children
/// are simply not signal-reaped (their `Drop` tree-kill still applies).
const CAPACITY: usize = 64;

/// Live child PIDs; 0 marks an empty slot (PID 0 is never a real child).
static CHILD_PIDS: [AtomicU32; CAPACITY] = [const { AtomicU32::new(0) }; CAPACITY];

/// Record a spawned emulator child.
pub(crate) fn register(pid: u32) {
    for slot in &CHILD_PIDS {
        if slot
            .compare_exchange(0, pid, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            return;
        }
    }
    // Table full: the child still gets its Drop tree-kill, only the
    // signal-reap coverage is lost. Not worth failing a spawn over.
}

/// Forget a child that is being torn down normally.
pub(crate) fn unregister(pid: u32) {
    for slot in &CHILD_PIDS {
        if slot
            .compare_exchange(pid, 0, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            return;
        }
    }
}

/// Kill one child's whole process tree, best effort. On unix the emulator was
/// spawned into its own process group (`process_group(0)`), so `kill(-pid)`
/// reaches the emulator and anything it forked; the direct-pid kill is the
/// fallback for the (never observed) case where the group setup failed. On
/// Windows `taskkill /T /F` walks the tree, the same pattern the dependency
/// installer's timeout kill uses.
pub(crate) fn kill_tree(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
        libc::kill(pid as i32, libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .output();
    }
}

/// Kill every registered child's tree and empty the table. Returns how many
/// children were killed. Public so a host binary can flush emulators on its
/// own shutdown path; the signal handler does the same thing inline (it
/// cannot call this: `taskkill` spawning is not async-signal-safe, and the
/// handler is unix-only anyway).
pub fn kill_all_registered() -> usize {
    let mut killed = 0;
    for slot in &CHILD_PIDS {
        let pid = slot.swap(0, Ordering::AcqRel);
        if pid != 0 {
            kill_tree(pid);
            killed += 1;
        }
    }
    killed
}

/// The unix signal handler: group-kill every registered child, then restore
/// the default disposition and re-raise, so the process still dies with the
/// correct signal status. Only async-signal-safe calls.
#[cfg(unix)]
extern "C" fn reap_and_die(sig: libc::c_int) {
    for slot in &CHILD_PIDS {
        let pid = slot.load(Ordering::Acquire);
        if pid != 0 {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
    }
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

/// Install the reaper for SIGTERM, SIGINT and SIGHUP (unix). Call once, early
/// in `main`, from any binary that can spawn emulators. Terminal behaviour is
/// unchanged: the handler ends by re-raising the signal with the default
/// disposition, so exit statuses and shell job control look exactly as
/// before; the only difference is that no emulator survives the parent.
///
/// On Windows this is a no-op: there are no POSIX signals to catch, `Drop`
/// already tree-kills via `taskkill /T`, and making a hard `TerminateProcess`
/// of the parent cascade needs a Job object, which is future native-Windows
/// work (tracked in docs/about/release-and-licensing.md section 5).
pub fn install_signal_reaper() {
    #[cfg(unix)]
    unsafe {
        let handler = reap_and_die as extern "C" fn(libc::c_int);
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::signal(sig, handler as libc::sighandler_t);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry is global to the process, so tests that flush it must not
    /// interleave.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// register/unregister round-trips through the fixed table, and
    /// kill_all empties it.
    #[test]
    fn registry_bookkeeping() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        // PIDs nothing real can hold during the test: register/unregister
        // only touch the table, and kill_tree on a nonexistent pid is a
        // harmless ESRCH.
        let a = 4_000_000_001u32;
        let b = 4_000_000_002u32;
        register(a);
        register(b);
        unregister(a);
        // Only b is still registered; kill_all reports exactly it.
        let killed = kill_all_registered();
        assert_eq!(killed, 1, "one live registration expected");
        assert_eq!(kill_all_registered(), 0, "table is empty after the flush");
    }

    /// kill_all_registered really kills a live child (its own process group
    /// on unix). The full tree semantics, grandchildren included, are
    /// covered by tests/child_reaping.rs against the real spawn paths.
    #[cfg(unix)]
    #[test]
    fn kill_all_kills_a_live_child() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("300");
        cmd.process_group(0);
        let mut child = cmd.spawn().expect("spawn sleep");
        register(child.id());
        assert!(kill_all_registered() >= 1);
        // SIGKILL is not blockable: wait() must report a signal death.
        let status = child.wait().expect("child reaped");
        assert!(!status.success(), "child was killed, not exited: {status}");
    }
}
