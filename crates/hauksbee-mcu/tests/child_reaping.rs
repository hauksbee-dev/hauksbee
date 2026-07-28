//! Emulator children must die with their owner, in both teardown shapes:
//!
//! 1. `Drop` kills the whole process TREE (the emulator plus anything it
//!    forked), not just the direct child.
//! 2. A fatal signal to the parent process (where `Drop` never runs) still
//!    reaps every registered emulator, via the signal reaper in
//!    `hauksbee_mcu::children`.
//!
//! The regression: killing a `hauksbee serve` with live co-sim sessions
//! orphaned its `qemu-system-*` children; 23 had accumulated on one dev
//! machine.
//!
//! Everything here is `#[cfg(unix)]`: the fake emulators are `sh` scripts and
//! the kill-the-parent scenario needs POSIX signals. The Windows side of the
//! same code path is `taskkill /T` on Drop, which only a native Windows
//! runner can exercise (tracked in docs/about/release-and-licensing.md
//! section 5); it is not silently skipped, it is not compiled here.

#![cfg(unix)]
#![cfg(feature = "qemu")]

use std::path::Path;
use std::time::{Duration, Instant};

/// Env var routing: when set, the `reaper_helper_process` test below acts as
/// the "parent that gets killed" instead of a real test.
const HELPER_FLAG: &str = "HAUKSBEE_TEST_REAPER_HELPER";
/// Where the fake emulator writes its forked grandchild's pid.
const GRANDCHILD_PID_FILE: &str = "HAUKSBEE_TEST_GRANDCHILD_PID_FILE";

/// Both real tests mutate process-wide env vars (`HAUKSBEE_QEMU_XTENSA`, the
/// pid-file path) before spawning, so they must not interleave.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// True while the OS still knows the pid (signal 0 probes existence).
fn alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Poll until `pid` is gone, up to a timeout. SIGKILL delivery is fast; the
/// generous ceiling only guards a loaded CI box.
fn assert_dies(pid: u32, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive(pid) {
        assert!(
            Instant::now() < deadline,
            "{what} (pid {pid}) is still alive long after the kill"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Write a fake `qemu-system-xtensa`: answers the `-machine help` fork probe
/// with an esp32 line, and otherwise forks a grandchild (recording its pid)
/// and becomes a long sleep, i.e. the same tree shape a real emulator run has.
fn write_fake_qemu(dir: &Path) -> std::path::PathBuf {
    let bin = dir.join("qemu-system-xtensa");
    std::fs::write(
        &bin,
        format!(
            "#!/bin/sh\n\
             if [ \"$1\" = \"-machine\" ]; then echo 'esp32 fake machine'; exit 0; fi\n\
             sleep 300 &\n\
             echo $! > \"${{{GRANDCHILD_PID_FILE}}}\"\n\
             exec sleep 300\n"
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

/// Read the grandchild pid the fake emulator recorded, waiting for the file.
fn read_grandchild_pid(path: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(s) = std::fs::read_to_string(path) {
            if let Ok(pid) = s.trim().parse() {
                return pid;
            }
        }
        assert!(
            Instant::now() < deadline,
            "fake emulator never wrote its grandchild pid to {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Shape 1: dropping a QemuProcess kills the emulator AND its forked
/// grandchild (the process-group kill), not just the direct child.
#[test]
fn drop_kills_the_whole_emulator_tree() {
    if std::env::var_os(HELPER_FLAG).is_some() {
        return; // running inside the helper re-exec; not this test's turn
    }
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let fake = write_fake_qemu(dir.path());
    let pid_file = dir.path().join("grandchild.pid");

    std::env::set_var("HAUKSBEE_QEMU_XTENSA", &fake);
    std::env::set_var(GRANDCHILD_PID_FILE, &pid_file);

    let proc = hauksbee_mcu::qemu::QemuProcess::spawn(
        hauksbee_mcu::qemu::QemuArch::Xtensa,
        "esp32",
        &dir.path().join("fake-flash.bin"),
        0,
        4461,
        4462,
    )
    .expect("fake emulator spawns");
    let emulator_pid = proc.pid();
    let grandchild_pid = read_grandchild_pid(&pid_file);
    assert!(alive(emulator_pid), "emulator runs before the drop");
    assert!(alive(grandchild_pid), "grandchild runs before the drop");

    drop(proc);

    assert_dies(emulator_pid, "the emulator");
    assert_dies(grandchild_pid, "the emulator's forked grandchild");
}

/// Shape 2: SIGTERM to the OWNING PROCESS reaps the emulator tree even
/// though no `Drop` ever runs. The parent-that-gets-killed is this same test
/// binary re-executed with `HAUKSBEE_TEST_REAPER_HELPER` set, filtered to
/// `reaper_helper_process`; it spawns the fake emulator through the real
/// `QemuProcess::spawn` path (which registers it with the signal reaper),
/// reports the pids on stdout, and then blocks until the SIGTERM arrives.
#[test]
fn sigterm_to_the_owner_reaps_the_emulator() {
    if std::env::var_os(HELPER_FLAG).is_some() {
        return;
    }
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let fake = write_fake_qemu(dir.path());
    let pid_file = dir.path().join("grandchild.pid");

    let exe = std::env::current_exe().unwrap();
    let mut helper = std::process::Command::new(exe)
        .args(["reaper_helper_process", "--exact", "--nocapture"])
        .env(HELPER_FLAG, "1")
        .env("HAUKSBEE_QEMU_XTENSA", &fake)
        .env(GRANDCHILD_PID_FILE, &pid_file)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("helper process spawns");

    // The helper prints SPAWNED <pid> once the emulator is up.
    let emulator_pid = {
        use std::io::BufRead;
        let stdout = helper.stdout.take().unwrap();
        let mut pid = None;
        for line in std::io::BufReader::new(stdout).lines() {
            let line = line.unwrap_or_default();
            if let Some(rest) = line.strip_prefix("SPAWNED ") {
                pid = rest.trim().parse::<u32>().ok();
                break;
            }
        }
        pid.expect("helper reported the emulator pid")
    };
    let grandchild_pid = read_grandchild_pid(&pid_file);
    assert!(
        alive(emulator_pid),
        "emulator runs before the parent is killed"
    );

    // Kill the OWNER, not the emulator. Default SIGTERM disposition would
    // never run Drop; only the installed reaper can save the children.
    unsafe { libc::kill(helper.id() as i32, libc::SIGTERM) };
    let status = helper.wait().expect("helper reaped");
    assert!(!status.success(), "helper died from the signal: {status}");

    assert_dies(emulator_pid, "the orphan-candidate emulator");
    assert_dies(grandchild_pid, "the orphan-candidate grandchild");
}

/// Not a test: the re-exec body for `sigterm_to_the_owner_reaps_the_emulator`.
/// Under plain `cargo test` (no `HAUKSBEE_TEST_REAPER_HELPER`) it is an
/// immediate no-op pass.
#[test]
fn reaper_helper_process() {
    if std::env::var_os(HELPER_FLAG).is_none() {
        return;
    }
    hauksbee_mcu::children::install_signal_reaper();
    let flash = std::env::temp_dir().join("hauksbee-reaper-helper-flash.bin");
    let proc = hauksbee_mcu::qemu::QemuProcess::spawn(
        hauksbee_mcu::qemu::QemuArch::Xtensa,
        "esp32",
        &flash,
        0,
        4463,
        4464,
    )
    .expect("fake emulator spawns in helper");
    println!("SPAWNED {}", proc.pid());
    use std::io::Write;
    std::io::stdout().flush().ok();
    // Hold the emulator alive until the parent SIGTERMs us. The reaper's
    // re-raise makes this process die of SIGTERM without ever unwinding, so
    // `proc`'s Drop never runs: exactly the leak scenario under test.
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}
