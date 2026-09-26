//! Fixture lookup shared by every integration-test module in this binary.
//!
//! The firmware images the emulator-backed tests boot are committed under
//! `testdata/firmware/` at the workspace root, but each one is BUILT by its own
//! `make`/`build.sh`, so a fresh checkout has the sources and not the images. A
//! test that cannot find its fixture must skip with a message naming what to
//! build, never fail, which is why every lookup returns `Option` rather than a
//! path: absence is a normal, reportable state here.

use std::path::PathBuf;

/// Serialize every test that touches Espressif QEMU: the ones that boot an
/// instance, and the ones that point the locator at a fake binary through
/// `HAUKSBEE_QEMU_XTENSA`. Since this binary links every module, an override
/// set by one test is visible to a booting test on another thread, which then
/// finds a "QEMU" that cannot boot and fails instead of skipping. One lock
/// across both kinds closes that, and keeps the real boots back-to-back so
/// their wall-clock behaviour stays predictable.
#[cfg(feature = "qemu")]
pub fn qemu_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Absolute path to `testdata/firmware/<rel>`, or `None` when that fixture has
/// not been built. Canonicalized where possible so a backend that resolves
/// relative paths against its own working directory (Renode) still finds it.
pub fn firmware(rel: &str) -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/firmware")
        .join(rel);
    p.exists().then(|| p.canonicalize().unwrap_or(p))
}

/// Record the backend's UART bytes without changing callback or lock semantics.
#[cfg(any(feature = "avr", feature = "renode"))]
pub fn capture_uart(mcu: &mut impl hauksbee_mcu::Mcu) -> std::sync::Arc<std::sync::Mutex<Vec<u8>>> {
    let uart = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = uart.clone();
    mcu.on_uart(Box::new(move |b| sink.lock().unwrap().push(b)));
    uart
}

/// Emit one Intel-HEX record with a correct checksum.
#[cfg(feature = "avr")]
pub fn ihex_record(record_type: u8, addr: u16, data: &[u8]) -> String {
    use std::fmt::Write as _;
    let len = u8::try_from(data.len()).expect("an Intel-HEX record holds at most 255 bytes");
    let [addr_hi, addr_lo] = addr.to_be_bytes();
    let mut sum = len
        .wrapping_add(addr_hi)
        .wrapping_add(addr_lo)
        .wrapping_add(record_type);
    let mut line = format!(":{:02X}{:04X}{:02X}", data.len(), addr, record_type);
    for &b in data {
        sum = sum.wrapping_add(b);
        write!(line, "{b:02X}").unwrap();
    }
    write!(line, "{:02X}", (!sum).wrapping_add(1)).unwrap();
    line
}

/// A temp path no other process or run can collide with: `name`, this process
/// id and the current nanosecond, plus `suffix` (`""` for a directory).
///
/// Tests run concurrently in the same temp dir, so a fixed name is a race.
#[cfg(any(feature = "avr", feature = "renode"))]
pub fn unique_temp(name: &str, suffix: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "{name}-{}-{}{suffix}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

#[cfg(feature = "avr")]
fn hex_dir(tag: &str) -> PathBuf {
    let dir = unique_temp(&format!("hauksbee-{tag}"), "");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Write `program` (raw flash bytes, little-endian words, loaded at 0) to a
/// temp .hex under a dir tagged `tag`, and return its path.
#[cfg(feature = "avr")]
pub fn write_program_hex(tag: &str, name: &str, program: &[u8]) -> PathBuf {
    let path = hex_dir(tag).join(name);
    let mut text = String::new();
    for (addr, chunk) in (0u16..).step_by(16).zip(program.chunks(16)) {
        text.push_str(&ihex_record(0x00, addr, chunk));
        text.push('\n');
    }
    text.push_str(":00000001FF\n"); // EOF record
    std::fs::write(&path, text).unwrap();
    path
}

/// Write pre-built `records` to a temp .hex under a dir tagged `tag`, and
/// return its path.
#[cfg(feature = "avr")]
pub fn write_hex(tag: &str, name: &str, records: &[String]) -> PathBuf {
    let path = hex_dir(tag).join(name);
    let mut text = records.join("\n");
    text.push('\n');
    text.push_str(":00000001FF\n"); // EOF record
    std::fs::write(&path, text).unwrap();
    path
}

/// `true` when live Renode is installed; otherwise prints the standard skip
/// message and returns `false` so the caller can `return` immediately. Every
/// `renode_*` test that skips with plain "Renode not installed" (not one of
/// the few with an extra unlock condition or a different message) uses this.
#[cfg(feature = "renode")]
pub fn renode_or_skip() -> bool {
    if hauksbee_mcu::renode::is_available() {
        true
    } else {
        eprintln!("SKIP: Renode not installed");
        false
    }
}

/// Boot the esp32 blinky image under Espressif QEMU, or print why that is not
/// possible here and return `None` so the caller skips.
#[cfg(feature = "qemu")]
pub fn esp32_blinky_or_skip() -> Option<hauksbee_mcu::QemuBackend> {
    use hauksbee_mcu::qemu::{is_available, QemuArch};
    if !is_available(QemuArch::Xtensa) {
        eprintln!("SKIP: Espressif QEMU (qemu-system-xtensa) not installed");
        return None;
    }
    let Some(fw) = firmware("esp32_blinky/flash.bin") else {
        eprintln!("SKIP: flash.bin not built; run ./build.sh in testdata/firmware/esp32_blinky");
        return None;
    };
    Some(hauksbee_mcu::QemuBackend::esp32(&fw).expect("boot Espressif QEMU esp32"))
}
