//! Data-driven MCU/SoC descriptors (06-extensibility-sdk §2).
//!
//! The per-part `RenodeConfig`/`QemuConfig` constructors used to embed register
//! offsets, platform paths, and port maps in hand-written Rust; the single
//! largest hardcoded surface in the co-sim layer, and the home of the
//! F103-vs-F4 ODR-offset footgun. This module moves that data into reviewed
//! TOML: one `db/mcu/<part>.soc.toml` file per part, read through a single
//! validated path with fail-loud, named errors, mirroring `sensor_spec.rs`.
//!
//! # The shape (06 §2)
//!
//! ```toml
//! [soc]
//! backend = "renode"
//! machine = "f401"
//! platform_repl = "@platforms/cpus/stm32f4.repl"
//! cpu_path = "sysbus.cpu"
//! uart = "sysbus.usart2"
//! frequency_hz = 16_000_000
//! expected_e_machine = "EM_ARM"
//! mcu_label = "STM32F401 (ARM Cortex-M4)"
//! [[soc.ports]]
//! letter = "A"
//! peripheral = "gpioPortA"
//! odr_offset = 0x14
//! width = 16
//! [soc.i2c]
//! controllers = ["i2c1"]
//! [soc.spi]
//! controllers = ["spi1"]
//! ```
//!
//! # What the 2026-07-01 plan example predated
//!
//! The plan's illustrative shape omits several fields the *real* constructors
//! set, all carried by this schema so a descriptor reproduces its constructor
//! byte-for-byte:
//!   - `machine`, `mcu_label`, `frequency_hz`, always present on the structs.
//!   - `extra_setup` / `post_load_setup`; the FE310 bring-up footgun (PRCI
//!     clock tags + `{cpu} PC vinit`) lives in `post_load_setup`, not code.
//!   - `[soc.spi].extra_repl`; the STM32F103 SPI1-injection fragment.
//!   - `[[soc.adc]]`; the AdcChannelMap injection recipes that landed after the
//!     plan (05 §5.1). No shipped built-in uses them (the stock Renode platforms
//!     model no ADC, so the loud-drop path is correct), but the schema carries
//!     them so a board that knows where its counts land can inject purely as
//!     data. See [`AdcChannelSpec`].
//!
//! The plan example also wrote `sysbus.gpioPortA` / `platforms/...` (no `@`);
//! the shipped descriptors instead store the exact backend-facing strings the
//! constructors used (`gpioPortA`, `@platforms/...`); the backend prepends
//! `sysbus.` when polling and Renode resolves the `@`-path, so the equivalence
//! proof against the deleted constructors is byte-exact. The plan's *field
//! names* are honored; the *values* are whatever the backend consumes.
//!
//! # Resolution (06 §6.4: a new Renode MCU addable purely as data)
//!
//! [`SocConfig::resolve`] maps a `backend:part` spec (e.g. `"renode:stm32f103"`)
//! to a descriptor. The shipped parts are embedded via `include_str!` (the
//! binary stays self-contained, the file stays the single source of truth; the
//! `mcp4728.toml` precedent), and a user override directory is searched first so
//! a new part is added without recompiling. See [`SocConfig::resolve`].
//!
//! # What honestly stays Rust (06 §2)
//!
//! A wholly new emulator backend (a new [`Mcu`](crate::traits::Mcu) impl) and
//! simavr part support are NOT data: simavr's own part database does the work,
//! and a new backend is a trait implementation, not a descriptor. A descriptor
//! only configures the three backends that already exist.

use std::path::PathBuf;

/// A SoC-descriptor load or validation failure.
///
/// Each domain check is a named variant, not a generic serde message, so a bad
/// descriptor fails loud and specific (the `sensor_spec.rs` discipline). Only
/// genuine TOML syntax errors fall through to [`SocError::Parse`].
#[derive(Debug, thiserror::Error)]
pub enum SocError {
    /// TOML did not parse (syntax, or a field of the wrong scalar type).
    #[error("SoC descriptor TOML parse error: {0}")]
    Parse(#[from] toml::de::Error),

    /// `soc.backend` was not one of the known backends.
    #[error("unknown backend {0:?}: expected \"renode\" or \"qemu\"")]
    UnknownBackend(String),

    /// A descriptor's backend disagrees with the loader it was handed to (e.g.
    /// a `backend = \"qemu\"` file passed to `RenodeConfig::from_soc_toml`).
    #[error("backend mismatch: descriptor declares {found:?} but was loaded as {expected:?}")]
    BackendMismatch { expected: String, found: String },

    /// The descriptor names a backend this build was not compiled with.
    #[error("backend {0:?} is not compiled into this build (missing cargo feature)")]
    BackendDisabled(String),

    /// `soc.platform_repl` was missing or empty (Renode has nothing to load).
    #[error("soc.platform_repl must not be empty")]
    EmptyPlatform,

    /// `soc.expected_e_machine` was not a recognised `EM_*` name.
    #[error("unknown e_machine {0:?}: expected one of EM_ARM, EM_RISCV, EM_XTENSA, EM_AVR")]
    UnknownEMachine(String),

    /// `soc.arch` (QEMU) was not a recognised architecture.
    #[error("unknown QEMU arch {0:?}: expected \"xtensa\" or \"riscv32\"")]
    UnknownArch(String),

    /// A GPIO port/bank declared `width = 0`; it addresses no bits.
    #[error("port/bank {letter:?} has zero width; a GPIO port must have at least one bit")]
    ZeroWidthPort { letter: char },

    /// A GPIO port/bank declared `width > 32`. The engine observes a bank as a
    /// single `u32` word (edge detection shifts `1u32 << bit`), so a wider bank
    /// would overflow the shift; refuse it at load rather than panic on poll.
    #[error("port/bank {letter:?} width {width} exceeds 32; a GPIO bank maps onto one 32-bit word")]
    PortTooWide { letter: char, width: u8 },

    /// Two GPIO ports/banks claim the same letter; the engine keys on the
    /// letter, so the second would silently shadow the first.
    #[error("duplicate GPIO port/bank letter {0:?}: port letters must be unique")]
    DuplicatePortLetter(char),

    /// A bus (`i2c`/`spi`) lists the same controller name twice.
    #[error("duplicate {bus} controller {name:?}: controller names must be unique")]
    DuplicateController { bus: &'static str, name: String },

    /// An `[[soc.adc]]` entry set neither, or both, of `monitor_command` and
    /// `memory_word`. Exactly one injection form is required.
    #[error(
        "ADC channel {channel}: exactly one of `monitor_command` or `memory_word` \
         must be set (got {set} of 2)"
    )]
    AdcInjectAmbiguous { channel: u8, set: u8 },

    /// A resolution spec was not of the form `backend:part`.
    #[error("SoC descriptor spec {0:?} must be of the form \"backend:part\" (e.g. \"renode:stm32f103\")")]
    BadSpec(String),

    /// No descriptor was found for a `backend:part` spec (not in any override
    /// directory, not among the built-ins). `hint` carries the did-you-mean
    /// suggestion and the built-in list, pre-formatted by [`SocConfig::resolve`]
    /// so every wrapper (the scheduler's `anyhow` context included) shows the
    /// user the valid part tokens instead of a dead end.
    #[error("no SoC descriptor found for {spec:?} (searched {searched} and the built-ins).{hint}")]
    NotFound {
        spec: String,
        searched: String,
        hint: String,
    },

    /// A descriptor file existed but could not be read.
    #[error("reading SoC descriptor {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// An override-dir descriptor file existed for the requested part but
    /// failed to load, carries the path so the failing FILE is named, and the
    /// inner named validation error so the failing FIELD/CHECK is too. This is
    /// deliberately fatal for the resolution (fail loud): an invalid override
    /// is never silently skipped in favour of a lower-priority descriptor.
    #[error("invalid SoC descriptor {path}: {source}")]
    InvalidDescriptor {
        path: String,
        #[source]
        source: Box<SocError>,
    },
}

/// The emulator backend a descriptor targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Renode,
    Qemu,
}

impl Backend {
    /// Parse a backend token, erroring (loud, named) on anything unknown.
    pub fn parse(s: &str) -> Result<Backend, SocError> {
        match s {
            "renode" => Ok(Backend::Renode),
            "qemu" => Ok(Backend::Qemu),
            other => Err(SocError::UnknownBackend(other.to_string())),
        }
    }

    /// The canonical token for this backend (the `backend:` half of a spec).
    pub fn name(self) -> &'static str {
        match self {
            Backend::Renode => "renode",
            Backend::Qemu => "qemu",
        }
    }
}

// ── The `[soc]` header, backend-agnostic ─────────────────────────────────────

/// Minimal peek at just `soc.backend`, used to dispatch a descriptor to the
/// right backend loader without committing to a full backend-specific schema.
#[derive(Debug, serde::Deserialize)]
struct SocHeaderFile {
    soc: SocHeader,
}
#[derive(Debug, serde::Deserialize)]
struct SocHeader {
    backend: String,
}

/// Read a descriptor's declared backend without fully parsing it.
pub fn peek_backend(src: &str) -> Result<Backend, SocError> {
    let header: SocHeaderFile = toml::from_str(src)?;
    Backend::parse(&header.soc.backend)
}

// ── Renode descriptor schema (06 §2) ─────────────────────────────────────────

#[cfg(feature = "renode")]
mod renode_schema {
    use super::SocError;
    use crate::renode::{AdcChannelMap, AdcInject, PortMap, RenodeConfig};

    /// TOML root: `[soc]`.
    #[derive(Debug, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct RenodeSocFile {
        pub soc: RenodeSoc,
    }

    /// The Renode `[soc]` body. Field names follow 06 §2; values are the exact
    /// backend-facing strings (see the module docs on the `@`/`sysbus.` note).
    #[derive(Debug, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct RenodeSoc {
        pub backend: String,
        pub machine: String,
        pub platform_repl: String,
        pub cpu_path: String,
        #[serde(default)]
        pub uart: Option<String>,
        pub frequency_hz: u64,
        pub expected_e_machine: String,
        pub mcu_label: String,
        #[serde(default)]
        pub extra_setup: Vec<String>,
        #[serde(default)]
        pub post_load_setup: Vec<String>,
        // `PortMap` (letter/peripheral/odr_offset/width) already derives
        // Deserialize, so `[[soc.ports]]` maps straight onto it; the existing
        // derive does the mechanical parsing (06 §2: reuse the derives).
        #[serde(default)]
        pub ports: Vec<PortMap>,
        #[serde(default)]
        pub i2c: BusTable,
        #[serde(default)]
        pub spi: SpiTable,
        #[serde(default)]
        pub adc: Vec<AdcChannelSpec>,
    }

    #[derive(Debug, Default, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct BusTable {
        #[serde(default)]
        pub controllers: Vec<String>,
    }

    #[derive(Debug, Default, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct SpiTable {
        #[serde(default)]
        pub controllers: Vec<String>,
        #[serde(default)]
        pub extra_repl: Option<String>,
    }

    /// One `[[soc.adc]]` entry: an ADC channel injection recipe (05 §5.1),
    /// flattened for TOML readability. Exactly one of `monitor_command`
    /// (peripheral-model feed) or `memory_word` (RAM result-word write) is set.
    #[derive(Debug, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct AdcChannelSpec {
        pub channel: u8,
        pub full_scale_volts: f64,
        pub max_count: u32,
        #[serde(default)]
        pub monitor_command: Option<String>,
        #[serde(default)]
        pub memory_word: Option<u32>,
    }

    impl AdcChannelSpec {
        fn into_map(self) -> Result<AdcChannelMap, SocError> {
            let inject = match (self.monitor_command, self.memory_word) {
                (Some(cmd), None) => AdcInject::MonitorCommand(cmd),
                (None, Some(word)) => AdcInject::MemoryWord(word),
                (Some(_), Some(_)) => {
                    return Err(SocError::AdcInjectAmbiguous {
                        channel: self.channel,
                        set: 2,
                    })
                }
                (None, None) => {
                    return Err(SocError::AdcInjectAmbiguous {
                        channel: self.channel,
                        set: 0,
                    })
                }
            };
            Ok(AdcChannelMap {
                channel: self.channel,
                inject,
                full_scale_volts: self.full_scale_volts,
                max_count: self.max_count,
            })
        }
    }

    impl RenodeSoc {
        pub(super) fn into_config(self) -> Result<RenodeConfig, SocError> {
            // Backend must be renode (this is the renode loader). A "qemu" file
            // here is a mismatch; an unknown token is UnknownBackend.
            let backend = super::Backend::parse(&self.backend)?;
            if backend != super::Backend::Renode {
                return Err(SocError::BackendMismatch {
                    expected: "renode".to_string(),
                    found: self.backend,
                });
            }
            if self.platform_repl.trim().is_empty() {
                return Err(SocError::EmptyPlatform);
            }
            let expected_e_machine = crate::elf::e_machine_from_name(&self.expected_e_machine)
                .ok_or_else(|| SocError::UnknownEMachine(self.expected_e_machine.clone()))?;

            super::validate_ports(self.ports.iter().map(|p| (p.letter, p.width)))?;
            super::validate_controllers("i2c", &self.i2c.controllers)?;
            super::validate_controllers("spi", &self.spi.controllers)?;

            let adc_channels = self
                .adc
                .into_iter()
                .map(AdcChannelSpec::into_map)
                .collect::<Result<Vec<_>, _>>()?;

            Ok(RenodeConfig {
                machine: self.machine,
                platform: self.platform_repl,
                cpu: self.cpu_path,
                uart: self.uart,
                ports: self.ports,
                frequency_hz: self.frequency_hz,
                extra_setup: self.extra_setup,
                post_load_setup: self.post_load_setup,
                i2c_controllers: self.i2c.controllers,
                spi_controllers: self.spi.controllers,
                spi_extra_repl: self.spi.extra_repl,
                expected_e_machine,
                mcu_label: self.mcu_label,
                adc_channels,
            })
        }
    }
}

#[cfg(feature = "renode")]
impl crate::renode::RenodeConfig {
    /// Load a Renode config from a `*.soc.toml` descriptor (06 §2).
    ///
    /// Parses the plan's `[soc]` shape, validates it with named errors (unknown
    /// backend, empty `platform_repl`, overlapping/zero-width ports, duplicate
    /// controllers, unknown `expected_e_machine`), and constructs the config.
    /// A `backend = "qemu"` descriptor is refused with [`SocError::BackendMismatch`].
    pub fn from_soc_toml(src: &str) -> Result<crate::renode::RenodeConfig, SocError> {
        // Check the declared backend BEFORE the full (deny_unknown_fields) parse,
        // so a QEMU descriptor gets the clear BackendMismatch error rather than a
        // confusing "unknown field `arch`" from the strict renode schema.
        match peek_backend(src)? {
            Backend::Renode => {}
            Backend::Qemu => {
                return Err(SocError::BackendMismatch {
                    expected: "renode".to_string(),
                    found: "qemu".to_string(),
                })
            }
        }
        let file: renode_schema::RenodeSocFile = toml::from_str(src)?;
        file.soc.into_config()
    }
}

// ── QEMU descriptor schema (06 §2) ───────────────────────────────────────────

#[cfg(feature = "qemu")]
mod qemu_schema {
    use super::SocError;
    use crate::qemu::{GpioBank, QemuArch, QemuConfig};

    #[derive(Debug, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct QemuSocFile {
        pub soc: QemuSoc,
    }

    /// The QEMU `[soc]` body. The ESP32 family observes GPIO through a RAM
    /// mailbox (the fork's gpio model has no register read-back), so a bank
    /// carries `out_reg`/`in_reg` mailbox addresses rather than an ODR offset.
    #[derive(Debug, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct QemuSoc {
        pub backend: String,
        pub arch: String,
        pub machine: String,
        pub icount_shift: u8,
        pub frequency_hz: u64,
        pub expected_e_machine: String,
        pub mcu_label: String,
        // `GpioBank` (letter/out_reg/in_reg/width) already derives Deserialize.
        #[serde(default)]
        pub banks: Vec<GpioBank>,
        #[serde(default)]
        pub i2c: QemuBusTable,
    }

    #[derive(Debug, Default, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct QemuBusTable {
        #[serde(default)]
        pub buses: Vec<String>,
    }

    fn arch_from_name(s: &str) -> Result<QemuArch, SocError> {
        match s {
            "xtensa" => Ok(QemuArch::Xtensa),
            "riscv32" => Ok(QemuArch::Riscv32),
            other => Err(SocError::UnknownArch(other.to_string())),
        }
    }

    impl QemuSoc {
        pub(super) fn into_config(self) -> Result<QemuConfig, SocError> {
            let backend = super::Backend::parse(&self.backend)?;
            if backend != super::Backend::Qemu {
                return Err(SocError::BackendMismatch {
                    expected: "qemu".to_string(),
                    found: self.backend,
                });
            }
            let arch = arch_from_name(&self.arch)?;
            let expected_e_machine = crate::elf::e_machine_from_name(&self.expected_e_machine)
                .ok_or_else(|| SocError::UnknownEMachine(self.expected_e_machine.clone()))?;

            super::validate_ports(self.banks.iter().map(|b| (b.letter, b.width)))?;
            super::validate_controllers("i2c", &self.i2c.buses)?;

            Ok(QemuConfig {
                arch,
                machine: self.machine,
                banks: self.banks,
                icount_shift: self.icount_shift,
                frequency_hz: self.frequency_hz,
                expected_e_machine,
                mcu_label: self.mcu_label,
                i2c_buses: self.i2c.buses,
            })
        }
    }
}

#[cfg(feature = "qemu")]
impl crate::qemu::QemuConfig {
    /// Load a QEMU config from a `*.soc.toml` descriptor (06 §2). Same validated
    /// path as the Renode loader; a `backend = "renode"` descriptor is refused.
    pub fn from_soc_toml(src: &str) -> Result<crate::qemu::QemuConfig, SocError> {
        match peek_backend(src)? {
            Backend::Qemu => {}
            Backend::Renode => {
                return Err(SocError::BackendMismatch {
                    expected: "qemu".to_string(),
                    found: "renode".to_string(),
                })
            }
        }
        let file: qemu_schema::QemuSocFile = toml::from_str(src)?;
        file.soc.into_config()
    }
}

// ── Shared validation helpers ────────────────────────────────────────────────

/// Validate GPIO port/bank `(letter, width)` pairs: no zero-width port, no two
/// ports sharing a letter (which the engine keys on).
fn validate_ports(ports: impl Iterator<Item = (char, u8)>) -> Result<(), SocError> {
    let mut seen: Vec<char> = Vec::new();
    for (letter, width) in ports {
        if width == 0 {
            return Err(SocError::ZeroWidthPort { letter });
        }
        if width > 32 {
            return Err(SocError::PortTooWide { letter, width });
        }
        if seen.contains(&letter) {
            return Err(SocError::DuplicatePortLetter(letter));
        }
        seen.push(letter);
    }
    Ok(())
}

/// Validate a bus controller list has no duplicate names.
fn validate_controllers(bus: &'static str, controllers: &[String]) -> Result<(), SocError> {
    let mut seen: Vec<&str> = Vec::new();
    for c in controllers {
        if seen.contains(&c.as_str()) {
            return Err(SocError::DuplicateController {
                bus,
                name: c.clone(),
            });
        }
        seen.push(c);
    }
    Ok(())
}

// ── Resolution: a `backend:part` spec → a descriptor (06 §6.4) ───────────────

/// Built-in descriptors, embedded so the binary is self-contained while the
/// file stays the single source of truth (the `mcp4728.toml` precedent). Keyed
/// by the `backend:part` spec [`SocConfig::resolve`] accepts.
const EMBEDDED: &[(&str, &str)] = &[
    ("renode:stm32f103", include_str!("../db/mcu/stm32f103.soc.toml")),
    (
        "renode:stm32f4_discovery",
        include_str!("../db/mcu/stm32f4_discovery.soc.toml"),
    ),
    ("renode:nrf52840", include_str!("../db/mcu/nrf52840.soc.toml")),
    ("renode:sifive_fe310", include_str!("../db/mcu/sifive_fe310.soc.toml")),
    ("renode:rp2040", include_str!("../db/mcu/rp2040.soc.toml")),
    ("qemu:esp32", include_str!("../db/mcu/esp32.soc.toml")),
    ("qemu:esp32s3", include_str!("../db/mcu/esp32s3.soc.toml")),
    ("qemu:esp32c3", include_str!("../db/mcu/esp32c3.soc.toml")),
];

/// A resolved descriptor, ready to hand to the backend it names.
#[derive(Debug, Clone)]
pub enum SocConfig {
    #[cfg(feature = "renode")]
    Renode(crate::renode::RenodeConfig),
    #[cfg(feature = "qemu")]
    Qemu(crate::qemu::QemuConfig),
}

impl SocConfig {
    /// Parse a descriptor string, dispatching to the backend it declares.
    pub fn from_soc_toml(src: &str) -> Result<SocConfig, SocError> {
        match peek_backend(src)? {
            Backend::Renode => {
                #[cfg(feature = "renode")]
                {
                    Ok(SocConfig::Renode(crate::renode::RenodeConfig::from_soc_toml(src)?))
                }
                #[cfg(not(feature = "renode"))]
                {
                    Err(SocError::BackendDisabled("renode".to_string()))
                }
            }
            Backend::Qemu => {
                #[cfg(feature = "qemu")]
                {
                    Ok(SocConfig::Qemu(crate::qemu::QemuConfig::from_soc_toml(src)?))
                }
                #[cfg(not(feature = "qemu"))]
                {
                    Err(SocError::BackendDisabled("qemu".to_string()))
                }
            }
        }
    }

    /// Resolve a `backend:part` spec (e.g. `"renode:stm32f103"`) to a descriptor.
    ///
    /// Search order, highest priority first (mirroring the model library's
    /// `--models-dir`-over-builtin layering):
    ///   1. `$HAUKSBEE_MCU_DIR/<part>.soc.toml` (an explicit override directory),
    ///   2. `~/.config/hauksbee/mcu/<part>.soc.toml` (the user's descriptor dir),
    ///   3. the embedded built-in for the spec.
    ///
    /// A file found by (1) or (2) still declares its own `backend`, and the
    /// resulting config is dispatched by that declared backend, so the `part`
    /// half of the spec is a filename, and the `backend` half is validated
    /// against the descriptor: a `renode:mypart` spec that resolves to a
    /// `backend = "qemu"` file is a [`SocError::BackendMismatch`], never a
    /// silent backend swap. This is the "add a Renode MCU purely as data"
    /// path (06 §6.4): drop `mypart.soc.toml` in the override dir and resolve
    /// `renode:mypart`.
    ///
    /// Fail-loud contract: a descriptor file that EXISTS for the requested
    /// part is always parsed, and any validation error it carries propagates.
    /// An invalid override for a builtin name (say a typo'd field in a user
    /// `stm32f103.soc.toml`) therefore fails the resolution rather than being
    /// silently skipped in favour of the builtin.
    pub fn resolve(spec: &str) -> Result<SocConfig, SocError> {
        let (backend_tok, part) = spec
            .split_once(':')
            .ok_or_else(|| SocError::BadSpec(spec.to_string()))?;
        if part.is_empty() {
            return Err(SocError::BadSpec(spec.to_string()));
        }
        // The spec's backend half must itself be a known backend; validate it
        // up front so "reonde:stm32f103" is an UnknownBackend, not a NotFound.
        let expected = Backend::parse(backend_tok)?;

        // An EXPLICITLY-set override dir that does not exist is a silent
        // fallback trap: every descriptor the user thinks they installed is
        // skipped and the built-in loads instead. Warn loudly (the
        // auto-discovered ~/.config dir is legitimately absent on most
        // machines and stays quiet).
        if let Some(msg) = missing_env_dir_warning() {
            eprintln!("WARNING: {msg}");
        }

        // (1)+(2): override directories, highest priority first.
        for dir in override_dirs() {
            let path = dir.join(format!("{part}.soc.toml"));
            match std::fs::read_to_string(&path) {
                Ok(src) => {
                    // Check the DECLARED backend against the spec's token
                    // before the full parse: peek_backend is feature-independent,
                    // so a mismatched descriptor reports BackendMismatch even in
                    // a build where the declared backend's feature is disabled
                    // (from_soc_toml would say BackendDisabled, misleading for
                    // an invalid renode override that typo'd `backend = "qemu"`).
                    return peek_backend(&src)
                        .and_then(|declared| {
                            if declared != expected {
                                return Err(SocError::BackendMismatch {
                                    expected: expected.name().to_string(),
                                    found: declared.name().to_string(),
                                });
                            }
                            Self::from_soc_toml(&src)
                        })
                        .map_err(|source| SocError::InvalidDescriptor {
                            path: path.display().to_string(),
                            source: Box::new(source),
                        })
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(source) => {
                    return Err(SocError::Io {
                        path: path.display().to_string(),
                        source,
                    })
                }
            }
        }

        // (3): embedded built-in.
        if let Some((_, src)) = EMBEDDED.iter().find(|(k, _)| *k == spec) {
            // The embedded key IS the full `backend:part` spec, so the declared
            // backend matches `expected` by construction; no re-check needed.
            //
            // Fell through to the built-in even though an override dir HAS
            // other descriptors: hint at the exact filename the resolver
            // expected, so a mis-named file (`stm32f103-mine.soc.toml`) is a
            // one-line diagnosis rather than a silent built-in fallback.
            for msg in builtin_fallback_hints(part, &override_dirs()) {
                eprintln!("note: {msg}");
            }
            return Self::from_soc_toml(src);
        }

        let searched = override_dirs()
            .iter()
            .map(|d| d.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        Err(SocError::NotFound {
            spec: spec.to_string(),
            searched: if searched.is_empty() {
                "(no override dirs)".to_string()
            } else {
                searched
            },
            hint: not_found_hint(spec, &Self::builtin_specs()),
        })
    }

    /// The `backend:part` specs of every embedded built-in descriptor.
    pub fn builtin_specs() -> Vec<&'static str> {
        EMBEDDED.iter().map(|(k, _)| *k).collect()
    }

    /// The backend this resolved descriptor targets.
    pub fn backend(&self) -> Backend {
        match self {
            #[cfg(feature = "renode")]
            SocConfig::Renode(_) => Backend::Renode,
            #[cfg(feature = "qemu")]
            SocConfig::Qemu(_) => Backend::Qemu,
            // A build with no backend feature has no variants, but `&Self` is
            // still considered inhabited, so the match needs an arm to compile.
            #[cfg(not(any(feature = "renode", feature = "qemu")))]
            _ => unreachable!("SocConfig cannot be constructed without a backend feature"),
        }
    }
}

/// The user descriptor override directories, highest priority first.
///
/// `$HAUKSBEE_MCU_DIR` is the explicit override (the analogue of the model
/// library's `--models-dir`); `~/.config/hauksbee/mcu` is the standing user
/// directory. Both are optional; a build with neither uses only the embedded
/// built-ins.
fn override_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(d) = std::env::var_os("HAUKSBEE_MCU_DIR") {
        if !d.is_empty() {
            dirs.push(PathBuf::from(d));
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".config/hauksbee/mcu"));
    }
    dirs
}

/// The warning for a SET-but-missing `$HAUKSBEE_MCU_DIR`, or `None` when the
/// variable is unset/empty or the directory exists. Only the EXPLICIT env var
/// warns: the auto-discovered `~/.config/hauksbee/mcu` dir is legitimately
/// absent on most machines and must stay silent. Split from the eprintln so
/// the wording is unit-testable ([`env_dir_missing_warning`] is the pure core).
fn missing_env_dir_warning() -> Option<String> {
    let d = std::env::var_os("HAUKSBEE_MCU_DIR")?;
    if d.is_empty() {
        return None;
    }
    env_dir_missing_warning(&PathBuf::from(d))
}

/// Pure core of [`missing_env_dir_warning`]: warn iff `dir` is not a directory.
fn env_dir_missing_warning(dir: &std::path::Path) -> Option<String> {
    if dir.is_dir() {
        return None;
    }
    Some(format!(
        "HAUKSBEE_MCU_DIR is set to '{}' but that directory does not exist; \
         MCU descriptor overrides will NOT load and the embedded built-ins \
         will be used instead",
        dir.display()
    ))
}

/// When resolution is about to fall through to a BUILT-IN even though an
/// override directory contains other `*.soc.toml` descriptors, produce one
/// hint per such directory naming the exact filename the resolver expected
/// (`<part>.soc.toml`). This catches the mis-named-file trap: the user
/// installed a descriptor, the resolver never looked at it, and the built-in
/// silently won. Directories with no descriptors (or that don't exist) stay
/// silent, nothing there was plausibly meant to override.
fn builtin_fallback_hints(part: &str, dirs: &[PathBuf]) -> Vec<String> {
    let mut hints = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut others: Vec<String> = entries
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.ends_with(".soc.toml"))
            .collect();
        others.sort();
        if !others.is_empty() {
            hints.push(format!(
                "using the built-in '{part}' descriptor: {} contains [{}] but not \
                 '{part}.soc.toml' (the exact filename the resolver looks for)",
                dir.display(),
                others.join(", ")
            ));
        }
    }
    hints
}

/// Build the [`SocError::NotFound`] hint: a nearest-match did-you-mean over the
/// built-in specs plus the full valid list, so an unknown part token is a
/// one-step fix rather than a dead end.
fn not_found_hint(spec: &str, builtins: &[&'static str]) -> String {
    let mut hint = String::new();
    if let Some(near) = nearest_builtin(spec, builtins) {
        hint.push_str(&format!(" Did you mean \"{near}\"?"));
    }
    hint.push_str(&format!(
        " Available built-ins: {}. Run `hauksbee models list --builtin` to \
         list them, or add a `<part>.soc.toml` descriptor in $HAUKSBEE_MCU_DIR \
         or ~/.config/hauksbee/mcu (docs/extending/add-an-mcu-variant.md).",
        builtins.join(", ")
    ));
    hint
}

/// The built-in spec closest to `spec`, or `None` when nothing is plausibly
/// close. Comparison is on the `part` half; a same-backend candidate is
/// preferred. Ranking is longest-common-prefix first, then edit distance,
/// LCP-first is what makes `stm32f407` suggest `stm32f4_discovery` (LCP 7)
/// over `stm32f103` (LCP 5, despite the smaller edit distance), mirroring how
/// a human reads part families.
fn nearest_builtin(spec: &str, builtins: &[&'static str]) -> Option<&'static str> {
    let (backend, part) = spec.split_once(':').unwrap_or(("", spec));
    let part = part.to_ascii_lowercase();
    // Key = (usize::MAX - lcp, dist, backend_penalty): tuple order makes a
    // bigger LCP compare smaller, so `<` picks it first.
    let mut best: Option<(usize, usize, usize, &'static str)> = None;
    for cand in builtins {
        let (cb, cp) = cand.split_once(':').unwrap_or(("", cand));
        let cp = cp.to_ascii_lowercase();
        let lcp = part
            .chars()
            .zip(cp.chars())
            .take_while(|(a, b)| a == b)
            .count();
        let dist = levenshtein(&part, &cp);
        // Plausibility gate: share a meaningful family prefix, or be within a
        // small edit distance. Without it every garbage token gets a random
        // "did you mean".
        if lcp < 4 && dist > part.len().max(3) / 2 {
            continue;
        }
        let backend_penalty = usize::from(backend != cb);
        // Order: bigger LCP wins, then smaller distance, then same backend.
        let key = (usize::MAX - lcp, dist, backend_penalty);
        if best.map_or(true, |(a, b, c, _)| key < (a, b, c)) {
            best = Some((key.0, key.1, key.2, cand));
        }
    }
    best.map(|(_, _, _, c)| c)
}

/// Classic Levenshtein edit distance (the same helper the engine's net
/// did-you-mean uses; duplicated locally because hauksbee-mcu sits below the
/// engine in the crate graph).
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut cur = vec![0usize; m + 1];
    for i in 1..=n {
        cur[0] = i;
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[m]
}

#[cfg(test)]
mod not_found_honesty_tests {
    use super::{
        builtin_fallback_hints, env_dir_missing_warning, nearest_builtin, not_found_hint,
        SocConfig, SocError,
    };

    // U3 finding 4: an unknown part token must name the valid built-ins and
    // offer a nearest match, never a bare "not found" dead end.
    #[test]
    fn unknown_part_suggests_nearest_builtin_and_lists_them_all() {
        let builtins = SocConfig::builtin_specs();
        let hint = not_found_hint("renode:stm32f407", &builtins);
        assert!(
            hint.contains("Did you mean \"renode:stm32f4_discovery\"?"),
            "stm32f407 must suggest the F4 Discovery descriptor: {hint}"
        );
        for b in &builtins {
            assert!(hint.contains(b), "hint must list every built-in ({b}): {hint}");
        }
        assert!(
            hint.contains("hauksbee models list --builtin"),
            "hint must point at the listing command: {hint}"
        );
    }

    #[test]
    fn nearest_builtin_prefers_family_prefix_over_raw_edit_distance() {
        let builtins = SocConfig::builtin_specs();
        // LCP-first: stm32f4_discovery (LCP 7) beats stm32f103 (LCP 5) even
        // though f103 is fewer edits away from f407.
        assert_eq!(
            nearest_builtin("renode:stm32f407", &builtins),
            Some("renode:stm32f4_discovery")
        );
        // A near-typo of an exact name still resolves.
        assert_eq!(
            nearest_builtin("renode:nrf52480", &builtins),
            Some("renode:nrf52840")
        );
        // Garbage gets NO suggestion rather than a random one.
        assert_eq!(nearest_builtin("renode:zzz9", &builtins), None);
    }

    #[test]
    fn resolve_error_display_carries_the_hint() {
        // End-to-end: the real resolve() error Display (what the scheduler
        // wraps verbatim into its anyhow context) must carry the suggestion.
        let err = SocConfig::resolve("renode:stm32f407").unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, SocError::NotFound { .. }), "{msg}");
        assert!(msg.contains("renode:stm32f4_discovery"), "{msg}");
        assert!(msg.contains("hauksbee models list --builtin"), "{msg}");
    }

    // U3 finding 5: a SET-but-missing $HAUKSBEE_MCU_DIR warns; an existing one
    // does not (pure-core test, no env mutation).
    #[test]
    fn missing_explicit_override_dir_warns_and_existing_one_does_not() {
        let tmp = std::env::temp_dir().join(format!(
            "hauksbee-soc-test-missing-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        let w = env_dir_missing_warning(&tmp).expect("a missing dir must warn");
        assert!(w.contains("HAUKSBEE_MCU_DIR") && w.contains("does not exist"), "{w}");
        std::fs::create_dir_all(&tmp).unwrap();
        assert!(env_dir_missing_warning(&tmp).is_none(), "an existing dir must not warn");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // U3 finding 5: falling through to a built-in while the override dir holds
    // OTHER descriptors hints at the exact expected filename.
    #[test]
    fn builtin_fallback_names_the_expected_descriptor_filename() {
        let tmp = std::env::temp_dir().join(format!(
            "hauksbee-soc-test-fallback-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("stm32f103-mine.soc.toml"), "x").unwrap();
        let hints = builtin_fallback_hints("stm32f103", &[tmp.clone()]);
        assert_eq!(hints.len(), 1, "{hints:?}");
        assert!(
            hints[0].contains("stm32f103.soc.toml")
                && hints[0].contains("stm32f103-mine.soc.toml"),
            "hint must name both the expected filename and what IS there: {}",
            hints[0]
        );
        // An empty dir produces no hint (nothing there was meant to override).
        let empty = tmp.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(builtin_fallback_hints("stm32f103", &[empty]).is_empty());
        // A missing dir produces no hint either.
        assert!(builtin_fallback_hints(
            "stm32f103",
            &[tmp.join("no-such-subdir")]
        )
        .is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}

#[cfg(test)]
mod width_validation_tests {
    use super::{validate_ports, SocError};

    /// Round-8 #15: a GPIO bank/port wider than 32 bits is refused at load; the
    /// engine observes a bank as one u32 word and shifts `1u32 << bit`, so a
    /// width of 33+ would overflow the shift on the first poll.
    #[test]
    fn width_over_32_is_refused() {
        let err = validate_ports([('A', 40)].into_iter()).unwrap_err();
        assert!(matches!(err, SocError::PortTooWide { letter: 'A', width: 40 }));
        // 32 is the maximum legal width and must pass.
        assert!(validate_ports([('A', 32)].into_iter()).is_ok());
        // A normal 16-bit port still validates.
        assert!(validate_ports([('B', 16)].into_iter()).is_ok());
    }
}
