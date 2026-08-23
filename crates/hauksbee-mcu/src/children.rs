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
//! layers: process-group kills on Unix, owning kill-on-close Job Objects on
//! Windows, and
//! [`install_signal_reaper`] for Unix paths where `Drop` never runs.
//!
//! On Unix the registry is a fixed-capacity, lock-free table of PIDs because
//! the signal handler iterates it, and a signal handler must never take a lock
//! the interrupted thread might hold. Everything the handler touches is
//! async-signal-safe: atomic loads, `kill(2)`, `signal(2)`, `raise(2)`. Windows
//! instead retains duplicated Job handles behind an ordinary mutex; there is
//! no signal handler on that platform, and handles are stable identities.

#[cfg(unix)]
use std::sync::atomic::{AtomicU32, Ordering};

/// Owns the platform primitive that ties an emulator process tree to the
/// hauksbee process. Unix uses the process group established before spawn, so
/// there is no additional handle to retain. Windows needs a Job Object with
/// `KILL_ON_JOB_CLOSE`: unlike a best-effort `taskkill` in `Drop`, the kernel
/// closes this handle and terminates the whole tree even when hauksbee itself
/// is killed and no destructor runs.
pub struct ProcessTreeGuard {
    #[cfg(windows)]
    job: windows_sys::Win32::Foundation::HANDLE,
}

// Win32 kernel handles are process-wide and CloseHandle may be called from any
// thread. The guard has unique ownership and exposes no operation besides Drop,
// so moving it with the MCU backend is safe.
#[cfg(windows)]
unsafe impl Send for ProcessTreeGuard {}

impl ProcessTreeGuard {
    fn attach(child: &std::process::Child) -> std::io::Result<Self> {
        #[cfg(windows)]
        {
            use std::ffi::c_void;
            use std::mem::{size_of, zeroed};
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
            use windows_sys::Win32::System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
                SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            };

            // SAFETY: every Win32 call is checked; `job` is either closed on
            // the error path or exclusively owned by the returned guard.
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return Err(std::io::Error::last_os_error());
                }
                let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &limits as *const _ as *const c_void,
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ) == 0
                {
                    let error = std::io::Error::last_os_error();
                    CloseHandle(job);
                    return Err(error);
                }
                if AssignProcessToJobObject(job, child.as_raw_handle() as HANDLE) == 0 {
                    let error = std::io::Error::last_os_error();
                    CloseHandle(job);
                    return Err(error);
                }
                Ok(Self { job })
            }
        }
        #[cfg(not(windows))]
        {
            let _ = child;
            Ok(Self {})
        }
    }

    /// Terminate the exact process tree owned by this guard.
    ///
    /// Windows uses the retained Job Object handle, never a numeric PID that
    /// may already have been reaped and recycled. Unix teardown is performed
    /// by the caller's process-group kill; the guard is only a marker there.
    pub fn terminate(&self) -> std::io::Result<()> {
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::JobObjects::TerminateJobObject;
            // SAFETY: `job` remains owned and live until this guard's Drop.
            if unsafe { TerminateJobObject(self.job, 1) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

/// Spawn a backend under structural process-tree ownership.
///
/// Windows starts the direct child suspended, assigns the still-unexecuted
/// process to the kill-on-close Job Object, then resumes its primary thread.
/// This prevents child-created descendants from escaping; Win32's stable
/// `std::process::Command` API does not support atomic Job assignment, so a
/// parent killed in the narrow spawn-to-assignment interval can still leave
/// the suspended direct child. That bounded limitation is documented rather
/// than represented as an impossible guarantee. Unix callers establish their
/// process group on `cmd` before entering here; the guard is then a zero-sized
/// owner marker.
pub fn spawn_owned(
    cmd: &mut std::process::Command,
) -> std::io::Result<(std::process::Child, ProcessTreeGuard)> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;

        cmd.creation_flags(CREATE_SUSPENDED);
        let mut child = cmd.spawn()?;
        let guard = match ProcessTreeGuard::attach(&child) {
            Ok(guard) => guard,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        if let Err(error) = resume_primary_thread(child.id()) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        Ok((child, guard))
    }
    #[cfg(not(windows))]
    {
        let child = cmd.spawn()?;
        let guard = ProcessTreeGuard::attach(&child)?;
        Ok((child, guard))
    }
}

#[cfg(windows)]
fn resume_primary_thread(pid: u32) -> std::io::Result<()> {
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    // SAFETY: the snapshot/thread handles are checked and closed on every path;
    // THREADENTRY32 advertises its exact initialized size as required by Win32.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        let mut entry: THREADENTRY32 = zeroed();
        entry.dwSize = size_of::<THREADENTRY32>() as u32;
        let mut found = Thread32First(snapshot, &mut entry) != 0;
        while found && entry.th32OwnerProcessID != pid {
            found = Thread32Next(snapshot, &mut entry) != 0;
        }
        let enumeration_error = if found {
            None
        } else {
            Some(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no primary thread found for suspended process {pid}"),
            ))
        };
        CloseHandle(snapshot);
        if let Some(error) = enumeration_error {
            return Err(error);
        }
        let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
        if thread.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let previous = ResumeThread(thread);
        let error = if previous == u32::MAX {
            Some(std::io::Error::last_os_error())
        } else {
            None
        };
        CloseHandle(thread);
        if let Some(error) = error {
            return Err(error);
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for ProcessTreeGuard {
    fn drop(&mut self) {
        // SAFETY: `job` is the live handle exclusively owned by this guard.
        // KILL_ON_JOB_CLOSE makes this the fail-safe tree termination path.
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.job);
        }
    }
}

/// Far above the server's live-session cap. If it ever fills, extra children
/// are simply not signal-reaped (their `Drop` tree-kill still applies).
const CAPACITY: usize = 64;

/// Live child PIDs; 0 marks an empty slot (PID 0 is never a real child).
#[cfg(unix)]
static CHILD_PIDS: [AtomicU32; CAPACITY] = [const { AtomicU32::new(0) }; CAPACITY];

/// Windows can use an ordinary mutex because no signal handler reads this
/// registry there. Each value is an independently owned duplicate Job handle,
/// so teardown cannot race the backend guard into a closed/reused handle.
#[cfg(windows)]
static CHILD_JOBS: std::sync::Mutex<Vec<(u32, usize)>> = std::sync::Mutex::new(Vec::new());

/// Record a spawned emulator child.
pub(crate) fn register(pid: u32, guard: &ProcessTreeGuard) {
    #[cfg(unix)]
    {
        let _ = guard;
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
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let mut jobs = CHILD_JOBS.lock().unwrap_or_else(|error| error.into_inner());
        if jobs.len() < CAPACITY {
            let mut duplicate: HANDLE = std::ptr::null_mut();
            // SAFETY: source and target are the current process; on success
            // the registry exclusively owns `duplicate` until unregister or
            // kill_all_registered closes it.
            let copied = unsafe {
                DuplicateHandle(
                    GetCurrentProcess(),
                    guard.job,
                    GetCurrentProcess(),
                    &mut duplicate,
                    0,
                    0,
                    DUPLICATE_SAME_ACCESS,
                )
            };
            if copied != 0 {
                jobs.push((pid, duplicate as usize));
            }
        }
    }
}

/// Forget a child that is being torn down normally.
pub(crate) fn unregister(pid: u32) {
    #[cfg(unix)]
    {
        for slot in &CHILD_PIDS {
            if slot
                .compare_exchange(pid, 0, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
        }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        let mut jobs = CHILD_JOBS.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(index) = jobs.iter().position(|(registered, _)| *registered == pid) {
            let (_, job) = jobs.swap_remove(index);
            // SAFETY: this registry owns the duplicated handle.
            unsafe { CloseHandle(job as windows_sys::Win32::Foundation::HANDLE) };
        }
    }
}

/// Kill one child's whole process tree, best effort. On unix the emulator was
/// spawned into its own process group (`process_group(0)`), so `kill(-pid)`
/// reaches the emulator and anything it forked; the direct-pid kill is the
/// fallback for the (never observed) case where the group setup failed.
/// Windows must use the retained Job Object handle instead: a reaped numeric
/// PID is not a stable process identity and may already name an unrelated
/// process.
#[cfg(unix)]
pub(crate) fn kill_tree(pid: u32) {
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

/// Kill every registered child's tree and empty the table. Returns how many
/// children were killed. Public so a host binary can flush emulators on its
/// own shutdown path; the signal handler does the same thing inline (it
/// cannot call this: `taskkill` spawning is not async-signal-safe, and the
/// handler is unix-only anyway).
pub fn kill_all_registered() -> usize {
    #[cfg(unix)]
    {
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
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        let jobs = {
            let mut registered = CHILD_JOBS.lock().unwrap_or_else(|error| error.into_inner());
            std::mem::take(&mut *registered)
        };
        for (_, job) in &jobs {
            // SAFETY: each value is a live handle duplicate exclusively owned
            // by this drained registry entry.
            unsafe {
                let handle = *job as HANDLE;
                TerminateJobObject(handle, 1);
                CloseHandle(handle);
            }
        }
        jobs.len()
    }
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
/// On Windows this is a no-op because there are no POSIX signals to catch.
/// Every Renode/QEMU child instead owns a kill-on-close Job Object through
/// [`ProcessTreeGuard`]; the kernel closes it and kills the tree even when a
/// hard parent termination prevents `Drop` from running.
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
    #[cfg(unix)]
    #[test]
    fn registry_bookkeeping() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        // PIDs nothing real can hold during the test: register/unregister
        // only touch the table, and kill_tree on a nonexistent pid is a
        // harmless ESRCH.
        let a = 4_000_000_001u32;
        let b = 4_000_000_002u32;
        let guard = ProcessTreeGuard {};
        register(a, &guard);
        register(b, &guard);
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
        let guard = ProcessTreeGuard {};
        register(child.id(), &guard);
        assert!(kill_all_registered() >= 1);
        // SIGKILL is not blockable: wait() must report a signal death.
        let status = child.wait().expect("child reaped");
        assert!(!status.success(), "child was killed, not exited: {status}");
    }

    #[cfg(windows)]
    #[test]
    fn windows_job_kills_child_when_guard_closes() {
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "ping -n 30 127.0.0.1 > NUL"]);
        let (mut child, guard) = spawn_owned(&mut command).expect("spawn in kill-on-close job");
        drop(guard);
        let status = child.wait().expect("job-owned child is waitable");
        assert!(
            !status.success(),
            "closing the job must terminate the child"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_kill_all_uses_registered_job_handles() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "ping -n 30 127.0.0.1 > NUL"]);
        let (mut child, guard) = spawn_owned(&mut command).expect("spawn in kill-on-close job");
        register(child.id(), &guard);
        assert_eq!(kill_all_registered(), 1);
        let status = child.wait().expect("job-terminated child is waitable");
        assert!(!status.success());
    }

    #[cfg(windows)]
    #[test]
    fn windows_reaped_child_teardown_uses_owned_job() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "exit 0"]);
        let (mut child, guard) = spawn_owned(&mut command).expect("spawn in owned job");
        register(child.id(), &guard);
        let status = child.wait().expect("short child is reaped");
        assert!(status.success());
        unregister(child.id());

        // The direct child handle has already been reaped. Termination remains
        // safe because it addresses the original Job Object, not child.id().
        guard
            .terminate()
            .expect("terminating an empty retained job is harmless");
    }

    #[cfg(windows)]
    fn process_is_live(pid: u32) -> bool {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return false;
            }
            let mut code = 0;
            let live = GetExitCodeProcess(handle, &mut code) != 0 && code == STILL_ACTIVE as u32;
            CloseHandle(handle);
            live
        }
    }

    #[cfg(windows)]
    fn wait_for_pid_file(path: &std::path::Path) -> u32 {
        for _ in 0..100 {
            if let Ok(text) = std::fs::read_to_string(path) {
                if let Ok(pid) = text.trim().parse() {
                    return pid;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("grandchild PID was not written to {}", path.display());
    }

    /// Helper invoked in a separate test process by the hard-parent regression.
    #[cfg(windows)]
    #[test]
    #[ignore = "invoked as a subprocess by the Windows Job Object regression"]
    fn windows_job_parent_helper() {
        let marker = std::env::var("HAUKSBEE_WINDOWS_JOB_PID_FILE")
            .expect("helper receives PID marker path");
        let marker = marker.replace('\'', "''");
        let script = format!(
            "$p=Start-Process powershell -ArgumentList '-NoProfile','-Command','Start-Sleep 300' -PassThru; Set-Content -LiteralPath '{marker}' -Value $p.Id; Wait-Process -Id $p.Id"
        );
        let mut command = std::process::Command::new("powershell");
        command.args(["-NoProfile", "-Command", &script]);
        let (mut child, _guard) = spawn_owned(&mut command).expect("spawn helper-owned tree");
        let _ = child.wait();
    }

    #[cfg(windows)]
    #[test]
    fn windows_hard_parent_death_kills_immediate_grandchild() {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let marker = scratch.path().join("grandchild.pid");
        let mut parent = std::process::Command::new(std::env::current_exe().expect("test exe"))
            .args([
                "--exact",
                "children::tests::windows_job_parent_helper",
                "--ignored",
                "--nocapture",
            ])
            .env("HAUKSBEE_WINDOWS_JOB_PID_FILE", &marker)
            .spawn()
            .expect("spawn separate Job Object owner");
        let grandchild = wait_for_pid_file(&marker);
        assert!(process_is_live(grandchild), "grandchild starts live");

        parent.kill().expect("hard-terminate the owning process");
        parent.wait().expect("owning process is waitable");
        for _ in 0..100 {
            if !process_is_live(grandchild) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("grandchild {grandchild} survived hard parent termination");
    }
}
