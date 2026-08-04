//! Data-driven MCU/SoC descriptor loader tests (06-extensibility-sdk §2).
//!
//! Three proofs:
//!   1. EQUIVALENCE, every shipped `db/mcu/*.soc.toml` loads to a config
//!      byte-identical to the Rust constructor it replaces. This is the gate the
//!      constructors' hand-written bodies were deleted behind (06 §2: "verified
//!      by the existing backend tests before its Rust constructor is deleted").
//!   2. VALIDATION, each named-error category fires against a deliberately
//!      broken descriptor (unknown backend, empty platform, overlapping/zero-
//!      width ports, duplicate controllers, unknown e_machine, …).
//!   3. RESOLUTION, `SocConfig::resolve("backend:part")` finds the embedded
//!      built-ins, and a user descriptor dropped in `$HAUKSBEE_MCU_DIR` is added
//!      purely as data (06 §6.4).

#![cfg(all(feature = "renode", feature = "qemu"))]

use hauksbee_mcu::{QemuConfig, RenodeConfig, SocConfig, SocError};

/// Serializes the tests that mutate `HAUKSBEE_MCU_DIR`: the test harness runs
/// tests on parallel threads sharing one process environment, so two tests
/// setting/removing the variable would clobber each other mid-resolve.
static MCU_DIR_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

// The embedded descriptor sources, via the same include_str! the crate uses.
const STM32F103: &str = include_str!("../db/mcu/stm32f103.soc.toml");
const STM32F4: &str = include_str!("../db/mcu/stm32f4_discovery.soc.toml");
const NRF52840: &str = include_str!("../db/mcu/nrf52840.soc.toml");
const FE310: &str = include_str!("../db/mcu/sifive_fe310.soc.toml");
const RP2040: &str = include_str!("../db/mcu/rp2040.soc.toml");
const ESP32: &str = include_str!("../db/mcu/esp32.soc.toml");
const ESP32S3: &str = include_str!("../db/mcu/esp32s3.soc.toml");
const ESP32C3: &str = include_str!("../db/mcu/esp32c3.soc.toml");

// ── (1) EQUIVALENCE ──────────────────────────────────────────────────────────

#[test]
fn renode_descriptors_equal_constructors() {
    // Byte-equal at the struct level (RenodeConfig: PartialEq); the descriptor
    // reproduces every field the hand-written constructor set.
    assert_eq!(
        RenodeConfig::from_soc_toml(STM32F103).unwrap(),
        RenodeConfig::stm32f103(),
        "stm32f103.soc.toml must reproduce RenodeConfig::stm32f103()"
    );
    assert_eq!(
        RenodeConfig::from_soc_toml(STM32F4).unwrap(),
        RenodeConfig::stm32f4_discovery(),
        "stm32f4_discovery.soc.toml must reproduce RenodeConfig::stm32f4_discovery()"
    );
    assert_eq!(
        RenodeConfig::from_soc_toml(NRF52840).unwrap(),
        RenodeConfig::nrf52840(),
        "nrf52840.soc.toml must reproduce RenodeConfig::nrf52840()"
    );
    assert_eq!(
        RenodeConfig::from_soc_toml(FE310).unwrap(),
        RenodeConfig::sifive_fe310(),
        "sifive_fe310.soc.toml must reproduce RenodeConfig::sifive_fe310()"
    );
    assert_eq!(
        RenodeConfig::from_soc_toml(RP2040).unwrap(),
        RenodeConfig::rp2040(),
        "rp2040.soc.toml must reproduce RenodeConfig::rp2040()"
    );
}

#[test]
fn qemu_descriptors_equal_constructors() {
    assert_eq!(
        QemuConfig::from_soc_toml(ESP32).unwrap(),
        QemuConfig::esp32(),
        "esp32.soc.toml must reproduce QemuConfig::esp32()"
    );
    assert_eq!(
        QemuConfig::from_soc_toml(ESP32S3).unwrap(),
        QemuConfig::esp32s3(),
        "esp32s3.soc.toml must reproduce QemuConfig::esp32s3()"
    );
    assert_eq!(
        QemuConfig::from_soc_toml(ESP32C3).unwrap(),
        QemuConfig::esp32c3(),
        "esp32c3.soc.toml must reproduce QemuConfig::esp32c3()"
    );
}

/// The FE310's post_load_setup (the bring-up footgun) survives the round trip:
/// this is the field the 06 §2 example predated, and losing it would silently
/// break FE310 boot.
#[test]
fn fe310_post_load_setup_is_carried() {
    let c = RenodeConfig::from_soc_toml(FE310).unwrap();
    assert_eq!(c.post_load_setup.len(), 3);
    assert!(c.post_load_setup[2].contains("vinit"));
    assert!(c.post_load_setup[0].contains("PRCI_HFROSCCFG"));
}

/// The STM32F103's SPI1 moved from an `extra_repl` injection into the inline
/// platform itself (the HAL clock-tree platform defines it unconditionally),
/// so `extra_repl` must be ABSENT: injecting a second `spi1` at the same
/// sysbus address would collide with the platform's. The F4's is also absent.
#[test]
fn spi_extra_repl_is_absent_where_the_platform_defines_the_controller() {
    let f103 = RenodeConfig::from_soc_toml(STM32F103).unwrap();
    assert_eq!(f103.spi_extra_repl, None);
    assert!(
        f103.platform.contains("spi1: SPI.STM32SPI @ sysbus 0x40013000"),
        "spi1 lives in the inline platform now"
    );
    let f4 = RenodeConfig::from_soc_toml(STM32F4).unwrap();
    assert_eq!(f4.spi_extra_repl, None);
}

/// E33: the shipped stm32f103 descriptor carries the proven HAL clock-tree
/// platform inline (RCC ready bits, FLASH_ACR, DMA1, SPI1, IWDG, 72 MHz
/// timers, peripheral bit-banding) so stock F1 CubeMX HAL firmware boots
/// instead of spinning forever in HAL_RCC_OscConfig on the stock platform.
#[test]
fn stm32f103_descriptor_ships_the_hal_boot_platform_inline() {
    let c = RenodeConfig::from_soc_toml(STM32F103).unwrap();
    assert!(
        c.platform.contains('\n'),
        "platform_repl is inline source, not a stock path"
    );
    assert!(
        c.platform.contains("using \"platforms/cpus/stm32f103.repl\""),
        "the inline platform extends the stock one"
    );
    for needle in [
        "rcc: Python.PythonPeripheral @ sysbus <0x40021000, +0x400>",
        "flashCtrl: Python.PythonPeripheral @ sysbus <0x40022000, +0x400>",
        "dma1: DMA.STM32G0DMA @ sysbus 0x40020000",
        "iwdg: Timers.STM32_IndependentWatchdog @ sysbus 0x40003000",
        "bitbandPeripherals: Miscellaneous.BitBanding @ sysbus <0x42000000, +0x2000000>",
        "frequency: 72000000",
    ] {
        assert!(c.platform.contains(needle), "platform must carry {needle}");
    }
    // The I2C single-read prefetch gate keys on the platform STRING containing
    // "stm32f1"; the `using` line keeps it firing for the inline form.
    assert!(c.platform.to_ascii_lowercase().contains("stm32f1"));
}

/// The AdcChannelMap schema (05 §5.1, post-plan) loads: no built-in uses it, but
/// a descriptor that carries an injection recipe must parse into the right map.
#[test]
fn adc_channel_recipe_loads_from_descriptor() {
    let src = r#"
[soc]
backend = "renode"
machine = "adc_demo"
platform_repl = "@platforms/cpus/stm32f103.repl"
cpu_path = "sysbus.cpu"
frequency_hz = 8_000_000
expected_e_machine = "EM_ARM"
mcu_label = "ADC demo"
[[soc.ports]]
letter = "C"
peripheral = "gpioPortC"
odr_offset = 0x0C
width = 16
[[soc.adc]]
channel = 0
full_scale_volts = 3.3
max_count = 4095
memory_word = 0x2000_4000
[[soc.adc]]
channel = 1
full_scale_volts = 1.8
max_count = 1023
monitor_command = "sysbus.adc FeedMillivolts {millivolts}"
"#;
    let c = RenodeConfig::from_soc_toml(src).unwrap();
    assert_eq!(c.adc_channels.len(), 2);
    assert_eq!(c.adc_channels[0].channel, 0);
    assert_eq!(c.adc_channels[0].max_count, 4095);
    assert_eq!(c.adc_channels[1].channel, 1);
    assert!((c.adc_channels[1].full_scale_volts - 1.8).abs() < 1e-9);
}

// ── (2) VALIDATION: each named-error category ────────────────────────────────

/// Build a minimal-but-valid Renode descriptor, then let the caller corrupt one
/// field, so each validation test isolates exactly one fault.
fn renode_descriptor(
    extra_ports: &str,
    i2c: &str,
    spi: &str,
    e_machine: &str,
    backend: &str,
) -> String {
    format!(
        r#"
[soc]
backend = "{backend}"
machine = "t"
platform_repl = "@platforms/cpus/stm32f103.repl"
cpu_path = "sysbus.cpu"
frequency_hz = 8_000_000
expected_e_machine = "{e_machine}"
mcu_label = "test"
{extra_ports}
{i2c}
{spi}
"#
    )
}

const ONE_PORT: &str =
    "[[soc.ports]]\nletter = \"A\"\nperipheral = \"gpioPortA\"\nodr_offset = 0x0C\nwidth = 16";

#[test]
fn unknown_backend_is_named() {
    let src = renode_descriptor(ONE_PORT, "", "", "EM_ARM", "banana");
    let err = SocConfig::from_soc_toml(&src).unwrap_err();
    assert!(
        matches!(err, SocError::UnknownBackend(ref b) if b == "banana"),
        "got: {err}"
    );
    assert!(err.to_string().contains("unknown backend"), "msg: {err}");
}

#[test]
fn empty_platform_repl_is_named() {
    let src = r#"
[soc]
backend = "renode"
machine = "t"
platform_repl = ""
cpu_path = "sysbus.cpu"
frequency_hz = 8_000_000
expected_e_machine = "EM_ARM"
mcu_label = "test"
[[soc.ports]]
letter = "A"
peripheral = "gpioPortA"
odr_offset = 0x0C
width = 16
"#;
    let err = RenodeConfig::from_soc_toml(src).unwrap_err();
    assert!(matches!(err, SocError::EmptyPlatform), "got: {err}");
    assert!(err.to_string().contains("platform_repl"), "msg: {err}");
}

#[test]
fn overlapping_port_letters_is_named() {
    let two_same = "[[soc.ports]]\nletter = \"A\"\nperipheral = \"gpioPortA\"\nodr_offset = 0x0C\nwidth = 16\n[[soc.ports]]\nletter = \"A\"\nperipheral = \"gpioPortB\"\nodr_offset = 0x0C\nwidth = 16";
    let src = renode_descriptor(two_same, "", "", "EM_ARM", "renode");
    let err = RenodeConfig::from_soc_toml(&src).unwrap_err();
    assert!(
        matches!(err, SocError::DuplicatePortLetter('A')),
        "got: {err}"
    );
    assert!(
        err.to_string().contains("duplicate GPIO port"),
        "msg: {err}"
    );
}

#[test]
fn zero_width_port_is_named() {
    let zero =
        "[[soc.ports]]\nletter = \"A\"\nperipheral = \"gpioPortA\"\nodr_offset = 0x0C\nwidth = 0";
    let src = renode_descriptor(zero, "", "", "EM_ARM", "renode");
    let err = RenodeConfig::from_soc_toml(&src).unwrap_err();
    assert!(
        matches!(err, SocError::ZeroWidthPort { letter: 'A' }),
        "got: {err}"
    );
    assert!(err.to_string().contains("zero width"), "msg: {err}");
}

#[test]
fn duplicate_i2c_controller_is_named() {
    let src = renode_descriptor(
        ONE_PORT,
        "[soc.i2c]\ncontrollers = [\"i2c1\", \"i2c1\"]",
        "",
        "EM_ARM",
        "renode",
    );
    let err = RenodeConfig::from_soc_toml(&src).unwrap_err();
    assert!(
        matches!(err, SocError::DuplicateController { bus: "i2c", ref name } if name == "i2c1"),
        "got: {err}"
    );
    assert!(
        err.to_string().contains("duplicate i2c controller"),
        "msg: {err}"
    );
}

#[test]
fn duplicate_spi_controller_is_named() {
    let src = renode_descriptor(
        ONE_PORT,
        "",
        "[soc.spi]\ncontrollers = [\"spi2\", \"spi2\"]",
        "EM_ARM",
        "renode",
    );
    let err = RenodeConfig::from_soc_toml(&src).unwrap_err();
    assert!(
        matches!(err, SocError::DuplicateController { bus: "spi", ref name } if name == "spi2"),
        "got: {err}"
    );
}

#[test]
fn unknown_e_machine_is_named() {
    let src = renode_descriptor(ONE_PORT, "", "", "EM_SPARC", "renode");
    let err = RenodeConfig::from_soc_toml(&src).unwrap_err();
    assert!(
        matches!(err, SocError::UnknownEMachine(ref m) if m == "EM_SPARC"),
        "got: {err}"
    );
    assert!(err.to_string().contains("unknown e_machine"), "msg: {err}");
}

#[test]
fn ambiguous_adc_inject_is_named() {
    // Both injection forms set → refuse (a descriptor field the backend cannot
    // honor two ways at once).
    let src = format!(
        "{}\n[[soc.adc]]\nchannel = 0\nfull_scale_volts = 3.3\nmax_count = 4095\nmemory_word = 0x2000_4000\nmonitor_command = \"x\"",
        renode_descriptor(ONE_PORT, "", "", "EM_ARM", "renode")
    );
    let err = RenodeConfig::from_soc_toml(&src).unwrap_err();
    assert!(
        matches!(err, SocError::AdcInjectAmbiguous { channel: 0, set: 2 }),
        "got: {err}"
    );
    // Neither form set → also refuse.
    let src2 = format!(
        "{}\n[[soc.adc]]\nchannel = 0\nfull_scale_volts = 3.3\nmax_count = 4095",
        renode_descriptor(ONE_PORT, "", "", "EM_ARM", "renode")
    );
    let err2 = RenodeConfig::from_soc_toml(&src2).unwrap_err();
    assert!(
        matches!(err2, SocError::AdcInjectAmbiguous { channel: 0, set: 0 }),
        "got: {err2}"
    );
}

#[test]
fn backend_mismatch_is_named() {
    // A qemu descriptor handed to the renode loader.
    let err = RenodeConfig::from_soc_toml(ESP32).unwrap_err();
    assert!(
        matches!(err, SocError::BackendMismatch { ref found, .. } if found == "qemu"),
        "got: {err}"
    );
}

#[test]
fn unknown_qemu_arch_is_named() {
    let src = r#"
[soc]
backend = "qemu"
arch = "sparc"
machine = "weird"
icount_shift = 2
frequency_hz = 240_000_000
expected_e_machine = "EM_XTENSA"
mcu_label = "weird"
[[soc.banks]]
letter = "0"
out_reg = 0x5000_0000
in_reg = 0x5000_0004
width = 32
"#;
    let err = QemuConfig::from_soc_toml(src).unwrap_err();
    assert!(
        matches!(err, SocError::UnknownArch(ref a) if a == "sparc"),
        "got: {err}"
    );
}

/// deny_unknown_fields: a mistyped field is a loud parse error, not a silent
/// drop (refuse rather than fake).
#[test]
fn unknown_field_is_rejected() {
    let src = renode_descriptor(ONE_PORT, "", "", "EM_ARM", "renode").replace(
        "mcu_label = \"test\"",
        "mcu_label = \"test\"\ntypoo_field = 1",
    );
    let err = RenodeConfig::from_soc_toml(&src).unwrap_err();
    assert!(matches!(err, SocError::Parse(_)), "got: {err}");
}

// ── (3) RESOLUTION ───────────────────────────────────────────────────────────

#[test]
fn resolve_embedded_builtins() {
    // Every built-in spec resolves and matches its constructor.
    match SocConfig::resolve("renode:stm32f103").unwrap() {
        SocConfig::Renode(c) => assert_eq!(c, RenodeConfig::stm32f103()),
        other => panic!("expected Renode, got {other:?}"),
    }
    match SocConfig::resolve("qemu:esp32c3").unwrap() {
        SocConfig::Qemu(c) => assert_eq!(c, QemuConfig::esp32c3()),
        other => panic!("expected Qemu, got {other:?}"),
    }
    // The full built-in set is discoverable.
    let specs = SocConfig::builtin_specs();
    assert!(specs.contains(&"renode:rp2040"));
    assert!(specs.contains(&"qemu:esp32"));
    assert_eq!(specs.len(), 8);
}

#[test]
fn resolve_bad_spec_and_missing() {
    assert!(matches!(
        SocConfig::resolve("no-colon").unwrap_err(),
        SocError::BadSpec(_)
    ));
    assert!(matches!(
        SocConfig::resolve("renode:nonesuch").unwrap_err(),
        SocError::NotFound { .. }
    ));
}

/// 06 §6.4 acceptance: a NEW Renode MCU added purely as data, drop a descriptor
/// in $HAUKSBEE_MCU_DIR and resolve it, no recompile. The override dir also wins
/// over a built-in of the same part name.
#[test]
fn resolve_from_override_dir_adds_a_part_as_data() {
    let dir = std::env::temp_dir().join(format!(
        "hauksbee-mcu-override-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    // A brand-new part that ships in no built-in: an "stm32f072" sibling.
    let new_part = r#"
[soc]
backend = "renode"
machine = "f072"
platform_repl = "@platforms/cpus/stm32f072.repl"
cpu_path = "sysbus.cpu"
uart = "sysbus.usart1"
frequency_hz = 8_000_000
expected_e_machine = "EM_ARM"
mcu_label = "STM32F072 (ARM Cortex-M0)"
[[soc.ports]]
letter = "A"
peripheral = "gpioPortA"
odr_offset = 0x14
width = 16
"#;
    std::fs::write(dir.join("stm32f072.soc.toml"), new_part).unwrap();

    // SAFETY (edition 2021): set_var is safe. The lock serializes this with
    // the other test mutating HAUKSBEE_MCU_DIR; the part resolved here is used
    // by no other test, so a concurrent resolve of a different part is
    // unaffected (it just won't find its file here).
    let _env = MCU_DIR_ENV.lock().unwrap();
    std::env::set_var("HAUKSBEE_MCU_DIR", &dir);
    let resolved = SocConfig::resolve("renode:stm32f072");
    std::env::remove_var("HAUKSBEE_MCU_DIR");
    std::fs::remove_dir_all(&dir).ok();

    match resolved.unwrap() {
        SocConfig::Renode(c) => {
            assert_eq!(c.machine, "f072");
            assert_eq!(c.mcu_label, "STM32F072 (ARM Cortex-M0)");
            assert_eq!(c.ports[0].odr_offset, 0x14);
        }
        other => panic!("expected Renode, got {other:?}"),
    }
}

/// The spec's `backend:` half is validated: an unknown backend token is a
/// named error up front (not a NotFound), and the embedded lookup is keyed by
/// the FULL spec, so a builtin part named under the wrong backend is NotFound
/// rather than a silent backend swap. (An override-dir file whose declared
/// backend disagrees with the spec is a BackendMismatch, see
/// `override_dir_is_fail_loud_and_beats_builtin`.)
#[test]
fn resolve_validates_the_spec_backend_token() {
    assert!(matches!(
        SocConfig::resolve("reonde:rp2040").unwrap_err(),
        SocError::UnknownBackend(_)
    ));
    assert!(matches!(
        SocConfig::resolve("qemu:rp2040").unwrap_err(),
        SocError::NotFound { .. }
    ));
}

/// The fail-loud contract on the override dirs (the fresh-context critic's
/// decisive probe): an INVALID override descriptor for a part about to be
/// used, including a BUILTIN part name, fails the resolution with the file
/// path and the named inner error. It is never silently skipped in favour of
/// the embedded builtin. And a VALID override for a builtin name WINS over
/// the builtin (the repo's layering doctrine).
#[test]
fn override_dir_is_fail_loud_and_beats_builtin() {
    let dir = std::env::temp_dir().join(format!(
        "hauksbee-mcu-shadow-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();

    // (a) Invalid override for the BUILTIN part nrf52840: a typo'd field the
    // deny_unknown_fields schema must reject. (nrf52840 is used because no
    // other test in this binary resolves it while the env var is set.)
    let invalid = NRF52840.replace("platform_repl =", "platform_rep =");
    assert_ne!(invalid, NRF52840, "typo replacement must have applied");
    std::fs::write(dir.join("nrf52840.soc.toml"), &invalid).unwrap();

    // (b) A qemu-declared descriptor under a name resolved as renode: the
    // backend-token check also fails loud, wrapped with the file path.
    std::fs::write(dir.join("misdeclared_part.soc.toml"), ESP32C3).unwrap();

    // SAFETY (edition 2021): set_var is safe. The lock serializes this with
    // the other test mutating HAUKSBEE_MCU_DIR; the parts touched here are
    // resolved by no other test in this binary.
    let _env = MCU_DIR_ENV.lock().unwrap();
    std::env::set_var("HAUKSBEE_MCU_DIR", &dir);
    let invalid_res = SocConfig::resolve("renode:nrf52840");
    let mismatch_res = SocConfig::resolve("renode:misdeclared_part");

    // (c) Valid override for the builtin name WINS: a marker label proves the
    // override file, not the embedded builtin, was loaded.
    let valid = NRF52840.replace(
        "mcu_label = \"nRF52840 (ARM Cortex-M4)\"",
        "mcu_label = \"nRF52840 (OVERRIDE-DIR COPY)\"",
    );
    let marker_applied = valid != *NRF52840;
    std::fs::write(dir.join("nrf52840.soc.toml"), &valid).unwrap();
    let valid_res = SocConfig::resolve("renode:nrf52840");

    std::env::remove_var("HAUKSBEE_MCU_DIR");
    std::fs::remove_dir_all(&dir).ok();

    // (a) asserts: named error carrying the path and the typo'd field.
    let err = invalid_res.unwrap_err();
    match &err {
        SocError::InvalidDescriptor { path, source } => {
            assert!(path.contains("nrf52840.soc.toml"), "path: {path}");
            assert!(matches!(**source, SocError::Parse(_)), "source: {source}");
            assert!(err.to_string().contains("platform_rep"), "err: {err}");
        }
        other => panic!("expected InvalidDescriptor, got {other:?}"),
    }

    // (b) asserts: the mismatch is loud and names the file.
    match invalid_backend_of(mismatch_res.unwrap_err()) {
        SocError::BackendMismatch { expected, found } => {
            assert_eq!(expected, "renode");
            assert_eq!(found, "qemu");
        }
        other => panic!("expected BackendMismatch, got {other:?}"),
    }

    // (c) asserts: the override's marker label came through.
    assert!(marker_applied, "marker replacement must have applied");
    match valid_res.unwrap() {
        SocConfig::Renode(c) => assert_eq!(c.mcu_label, "nRF52840 (OVERRIDE-DIR COPY)"),
        other => panic!("expected Renode, got {other:?}"),
    }
}

/// Unwrap an InvalidDescriptor to its inner error, for variant assertions.
fn invalid_backend_of(err: SocError) -> SocError {
    match err {
        SocError::InvalidDescriptor { source, .. } => *source,
        other => other,
    }
}
