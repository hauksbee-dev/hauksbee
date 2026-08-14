//! Data-driven MCU/SoC descriptors (06-extensibility-sdk §2).
//!
//! The register offsets, platform paths, and port maps behind the per-part
//! `RenodeConfig`/`QemuConfig` constructors are the largest part-specific
//! surface in the co-sim layer, and the home of the F103-vs-F4 ODR-offset
//! footgun. They live as reviewed TOML rather than as hand-written Rust: one
//! `db/mcu/<part>.soc.toml` file per part, read through a single validated
//! path with fail-loud, named errors, mirroring `sensor_spec.rs`.
//!
//! # The shape (06 §2)
//!
//! ```toml
//! [soc]
//! backend = "renode"
//! machine = "f401"
//! platform_repl = """
//! using "platforms/cpus/stm32f4.repl"
//!
//! nvic:
//!     systickFrequency: 16000000
//!
//! cpu:
//!     PerformanceInMips: 16
//! """
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
//!     `platform_repl` is inline source above rather than the plan's bare stock
//!     path, because a Renode part must declare its own core clock and be held
//!     to it: see `check_clock_declarations`.
//!   - `watchdog_limitation`, the per-part watchdog coverage statement, and
//!     `timing_limitation`, its per-part timing twin (the F103's deliberate
//!     TIMx-at-72MHz divergence).
//!   - `extra_setup` / `post_load_setup`; the FE310 bring-up footgun (PRCI
//!     clock tags + `{cpu} PC vinit`) lives in `post_load_setup`, not code.
//!   - `[soc.spi].extra_repl`; the STM32F103 SPI1-injection fragment.
//!   - `[[soc.adc]]`; the AdcChannelMap injection recipes that landed after the
//!     plan (05 §5.1). No shipped built-in uses them (the stock Renode platforms
//!     model no ADC, so the loud-drop path is correct), but the schema carries
//!     them so a board that knows where its counts land can inject purely as
//!     data. Each entry names the channel, its full-scale volts and max
//!     count, and exactly one of `monitor_command` or `memory_word`.
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

    /// `soc.frequency_hz` was 0. [`crate::traits::Mcu::run_cycles`] divides a
    /// cycle count by this value on both external backends, so a zero clock
    /// turns a bounded run window into an infinite one rather than erroring.
    #[error(
        "soc.frequency_hz must not be 0: a cycle count is divided by it to get a \
         run window, so a zero clock makes that window infinite"
    )]
    ZeroFrequency,

    /// The platform declares a core clock that disagrees with
    /// `soc.frequency_hz`. This is the error the whole clock cross-check exists
    /// for: four shipped platforms declared a rate 4.5x to 9x off the part's,
    /// simulated time ran at the emulator's clock rate instead of the part's,
    /// and nothing complained because `frequency_hz` cancels out of the
    /// engine's own `cycles = seconds * frequency_hz` bookkeeping.
    #[error(
        "soc.frequency_hz is {frequency_hz} Hz but the platform declares \
         {property}: {declared}, which is {expected_prose}. Simulated time \
         would run at {ratio:.3}x the part's real rate. Fix whichever is wrong; \
         they describe the same clock."
    )]
    ClockMismatch {
        /// The `.repl` property that disagrees.
        property: &'static str,
        /// The value the platform declares.
        declared: u64,
        /// The descriptor's declared part clock.
        frequency_hz: u64,
        /// What the property would have to be to agree, in words.
        expected_prose: String,
        /// Sim rate over the part's real rate, the quantity the clock-truth
        /// gate measures, so the error and the test speak the same units.
        ratio: f64,
    },

    /// A Renode descriptor whose platform declares no core clock at all.
    ///
    /// Refused rather than defaulted, because "no declaration" is precisely how
    /// the nRF52840 came to run 6.58x fast: with neither `PerformanceInMips` nor
    /// `systickFrequency` given, both fall to Renode defaults that have nothing
    /// to do with the part, and a descriptor pointing straight at a stock
    /// `@platforms/...` file cannot state a clock at all. `platform_repl`
    /// accepts inline `.repl` source, so extending the stock platform with
    /// `using "platforms/..."` plus the two declarations costs three lines and
    /// makes the claim checkable.
    #[error(
        "the platform for this Renode part declares no core clock, so Renode \
         picks its own and simulated time has no relation to the part. Make \
         platform_repl inline source (a `using \"{platform}\"` line plus \
         `cpu PerformanceInMips: {mips}` and, on a Cortex-M part, \
         `nvic systickFrequency: {frequency_hz}`) so the declared clock can be \
         checked against soc.frequency_hz"
    )]
    UndeclaredClock {
        /// The platform reference the descriptor currently uses, so the error
        /// can quote the `using` line the author needs.
        platform: String,
        /// `frequency_hz` in MHz, the value `PerformanceInMips` wants.
        mips: u64,
        /// The descriptor's declared part clock.
        frequency_hz: u64,
    },

    /// An identifier field was present but blank. `platform_repl` has its own
    /// [`SocError::EmptyPlatform`]; these are the fields that reach the
    /// emulator as an empty Monitor or command-line argument, where the failure
    /// arrives seconds later and names the emulator rather than the descriptor.
    #[error("soc.{field} must not be empty: it reaches the emulator as an empty argument")]
    EmptyField { field: &'static str },

    /// A `{support}` token with no `support_bundle` to substitute it with. The
    /// token is left verbatim at bring-up on purpose (a path error naming the
    /// token beats a missing file at `/rp2040.repl`), but a descriptor that
    /// declares no bundle can never have it substituted at all, so the whole
    /// field is unusable and the honest place to say so is here.
    #[error(
        "soc.{field} uses the `{{support}}` token but no `support_bundle` is declared, \
         so the token can never be substituted"
    )]
    SupportTokenWithoutBundle { field: &'static str },

    /// A descriptor clock-control command omitted the value the backend must
    /// substitute per board or per virtual-time slice.
    #[error(
        "soc.clock_control.{field} must contain {placeholder}: without it the clock model cannot vary with board presence or virtual time"
    )]
    ClockControlTemplate {
        field: &'static str,
        placeholder: &'static str,
    },

    /// A 16-pin direction encoding on a port wider than 16 bits. `moder` and
    /// `stm32f1_crl_crh` decode 16 pins by construction, so the top pins read
    /// as inputs and every edge on them is suppressed: strictly worse than no
    /// `dir` map, which at least reports every output-state change.
    #[error(
        "port {letter:?} is {width} bits wide but its dir encoding {encoding:?} decodes \
         only 16 pins; pins 16 and above would read as inputs and their edges would be \
         dropped silently. Use \"dir_bits\", or narrow the port"
    )]
    DirEncodingTooNarrow {
        letter: char,
        width: u8,
        encoding: &'static str,
    },

    /// Two `[[soc.adc]]` entries claim the same channel. The backend keys
    /// injection on the channel index, so the second recipe shadows the first.
    #[error("duplicate ADC channel {channel}: the second recipe would shadow the first")]
    DuplicateAdcChannel { channel: u8 },

    /// An `[[soc.adc]]` scaling factor that cannot scale. `max_count` and
    /// `full_scale_volts` are both divisors in the volts-to-count conversion; a
    /// zero or non-finite one converts every injected voltage to count 0, which
    /// reads as a stuck converter rather than a bad descriptor.
    #[error("ADC channel {channel}: {field} is {value}, which cannot scale a count")]
    AdcScaleInvalid {
        channel: u8,
        field: &'static str,
        value: String,
    },

    /// `soc.expected_e_machine` was not a recognised `EM_*` name.
    #[error("unknown e_machine {0:?}: expected one of EM_ARM, EM_RISCV, EM_XTENSA, EM_AVR")]
    UnknownEMachine(String),

    /// `soc.support_bundle` named a bundle this build does not carry. Caught at
    /// descriptor load rather than at machine bring-up: the bundle is embedded in
    /// the binary, so a name that is wrong now is wrong forever, and finding out
    /// only after Renode has been spawned wastes seconds and buries the reason.
    #[error("unknown support_bundle {name:?}: this build carries {known:?}")]
    UnknownSupportBundle { name: String, known: Vec<String> },

    /// `soc.arch` (QEMU) was not a recognised architecture.
    #[error("unknown QEMU arch {0:?}: expected \"xtensa\" or \"riscv32\"")]
    UnknownArch(String),

    /// A GPIO port/bank declared `width = 0`; it addresses no bits.
    #[error("port/bank {letter:?} has zero width; a GPIO port must have at least one bit")]
    ZeroWidthPort { letter: char },

    /// A GPIO port/bank declared `width > 32`. The engine observes a bank as a
    /// single `u32` word (edge detection shifts `1u32 << bit`), so a wider bank
    /// would overflow the shift; refuse it at load rather than panic on poll.
    #[error(
        "port/bank {letter:?} width {width} exceeds 32; a GPIO bank maps onto one 32-bit word"
    )]
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
    use crate::renode::{
        AdcChannelMap, AdcInject, ClockControl, DirEncoding, PortMap, RenodeConfig,
    };

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
        /// Optional named support bundle (peripheral models Renode does not
        /// ship) to load before the platform. See `renode::support`.
        #[serde(default)]
        pub support_bundle: Option<String>,
        pub cpu_path: String,
        #[serde(default)]
        pub uart: Option<String>,
        pub frequency_hz: u64,
        pub expected_e_machine: String,
        pub mcu_label: String,
        /// How this part's watchdog fidelity falls short, as a whole sentence
        /// rendered verbatim on the report surfaces. Omitted means "an armed,
        /// never-fed watchdog reboots the core the way silicon does", which is
        /// a claim, so it belongs to whoever measured it and the descriptor
        /// says which part they measured.
        #[serde(default)]
        pub watchdog_limitation: Option<String>,
        /// How this part's TIMING fidelity falls short, as a whole sentence
        /// rendered verbatim on the report surfaces. Omitted means "a firmware
        /// delay costs the virtual time it costs on silicon", which is a
        /// measured claim (tests/clock_truth.rs), same discipline as the
        /// watchdog field above.
        #[serde(default)]
        pub timing_limitation: Option<String>,
        #[serde(default)]
        pub extra_setup: Vec<String>,
        #[serde(default)]
        pub post_load_setup: Vec<String>,
        #[serde(default)]
        pub clock_control: Option<ClockControl>,
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
        /// The scaling checks, run before the injection form is resolved so a
        /// descriptor with both problems reports the arithmetic one first (it is
        /// the one that silently reads as a stuck converter).
        fn validate_scaling(&self) -> Result<(), SocError> {
            if self.max_count == 0 {
                return Err(SocError::AdcScaleInvalid {
                    channel: self.channel,
                    field: "max_count",
                    value: "0".to_string(),
                });
            }
            if !self.full_scale_volts.is_finite() || self.full_scale_volts <= 0.0 {
                return Err(SocError::AdcScaleInvalid {
                    channel: self.channel,
                    field: "full_scale_volts",
                    value: self.full_scale_volts.to_string(),
                });
            }
            Ok(())
        }

        fn into_map(self) -> Result<AdcChannelMap, SocError> {
            self.validate_scaling()?;
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

    /// The descriptor token for a direction encoding, and how many pins that
    /// encoding can decode. The pin counts are structural: `moder` reads 2 bits
    /// per pin and `stm32f1_crl_crh` 4 bits per pin out of one 32-bit word (plus
    /// a second word for CRH), so both top out at 16 pins whatever the port is.
    fn dir_encoding_reach(encoding: DirEncoding) -> (&'static str, u8) {
        match encoding {
            DirEncoding::Moder => ("moder", 16),
            DirEncoding::Stm32f1CrlCrh => ("stm32f1_crl_crh", 16),
            DirEncoding::DirBits => ("dir_bits", 32),
        }
    }

    /// Refuse a direction encoding that cannot cover the port it is on.
    ///
    /// A too-narrow encoding does not fail: it decodes the pins it reaches and
    /// leaves the rest reading as inputs, which suppresses every edge above the
    /// encoding's reach. That is silently worse than omitting `dir` entirely, so
    /// it is refused at load for the same reason a duplicate port letter is.
    fn validate_port_dir_widths(ports: &[PortMap]) -> Result<(), SocError> {
        for port in ports {
            let Some(dir) = port.dir else { continue };
            let (encoding, reach) = dir_encoding_reach(dir.encoding);
            if port.width > reach {
                return Err(SocError::DirEncodingTooNarrow {
                    letter: port.letter,
                    width: port.width,
                    encoding,
                });
            }
        }
        Ok(())
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
            super::validate_non_empty(&[("machine", &self.machine), ("cpu_path", &self.cpu_path)])?;
            // A present-but-blank `uart` asks the backend for
            // `connector Connect "" hauksbee_uart`. Omitting the field is how a
            // descriptor says "no UART bridge"; a blank one says nothing.
            if let Some(uart) = &self.uart {
                super::validate_non_empty(&[("uart", uart)])?;
            }
            super::validate_frequency(self.frequency_hz)?;
            if self.support_bundle.is_none() {
                let mut token_fields: Vec<(&'static str, bool)> = vec![
                    ("platform_repl", self.platform_repl.contains("{support}")),
                    (
                        "extra_setup",
                        self.extra_setup.iter().any(|c| c.contains("{support}")),
                    ),
                    (
                        "post_load_setup",
                        self.post_load_setup.iter().any(|c| c.contains("{support}")),
                    ),
                ];
                token_fields.retain(|(_, uses)| *uses);
                if let Some((field, _)) = token_fields.first() {
                    return Err(SocError::SupportTokenWithoutBundle { field });
                }
            }
            if let Some(name) = &self.support_bundle {
                if crate::renode::support::lookup(name).is_none() {
                    return Err(SocError::UnknownSupportBundle {
                        name: name.clone(),
                        known: crate::renode::support::known_names()
                            .into_iter()
                            .map(String::from)
                            .collect(),
                    });
                }
            }
            // The clock is one fact declared in two places; refuse a
            // descriptor whose two places disagree, or which declares none.
            super::check_clock_declarations(
                &self.platform_repl,
                self.support_bundle.as_deref(),
                self.frequency_hz,
            )?;
            let expected_e_machine = crate::elf::e_machine_from_name(&self.expected_e_machine)
                .ok_or_else(|| SocError::UnknownEMachine(self.expected_e_machine.clone()))?;

            super::validate_ports(self.ports.iter().map(|p| (p.letter, p.width)))?;
            validate_port_dir_widths(&self.ports)?;
            super::validate_controllers("i2c", &self.i2c.controllers)?;
            super::validate_controllers("spi", &self.spi.controllers)?;

            if let Some(clock) = &self.clock_control {
                super::validate_non_empty(&[
                    ("clock_control.presence_command", &clock.presence_command),
                    ("clock_control.tick_command", &clock.tick_command),
                ])?;
                if !clock.presence_command.contains("{present}") {
                    return Err(SocError::ClockControlTemplate {
                        field: "presence_command",
                        placeholder: "{present}",
                    });
                }
                if !clock.tick_command.contains("{micros}") {
                    return Err(SocError::ClockControlTemplate {
                        field: "tick_command",
                        placeholder: "{micros}",
                    });
                }
            }

            let mut seen_channels: Vec<u8> = Vec::new();
            for entry in &self.adc {
                if seen_channels.contains(&entry.channel) {
                    return Err(SocError::DuplicateAdcChannel {
                        channel: entry.channel,
                    });
                }
                seen_channels.push(entry.channel);
            }
            let adc_channels = self
                .adc
                .into_iter()
                .map(AdcChannelSpec::into_map)
                .collect::<Result<Vec<_>, _>>()?;

            Ok(RenodeConfig {
                machine: self.machine,
                platform: self.platform_repl,
                support_bundle: self.support_bundle,
                cpu: self.cpu_path,
                uart: self.uart,
                ports: self.ports,
                frequency_hz: self.frequency_hz,
                extra_setup: self.extra_setup,
                post_load_setup: self.post_load_setup,
                clock_control: self.clock_control,
                i2c_controllers: self.i2c.controllers,
                spi_controllers: self.spi.controllers,
                spi_extra_repl: self.spi.extra_repl,
                expected_e_machine,
                mcu_label: self.mcu_label,
                watchdog_limitation: self.watchdog_limitation,
                timing_limitation: self.timing_limitation,
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

    /// The QEMU `[soc]` body. A bank carries the real GPIO OUT address plus the
    /// legacy output/input mailbox addresses; `gpio_qom_path` is the live
    /// capability probe that decides whether the real register is trustworthy.
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
        pub gpio_qom_path: String,
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
            super::validate_non_empty(&[
                ("machine", &self.machine),
                ("gpio_qom_path", &self.gpio_qom_path),
            ])?;
            super::validate_frequency(self.frequency_hz)?;
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
                gpio_qom_path: self.gpio_qom_path,
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

/// Refuse a zero part clock.
///
/// The emulator clocks its own platform, so this field does not set the emulated
/// rate by itself, but it is not inert twice over: `Mcu::run_cycles` on both
/// external backends divides a cycle count by it to get the run window, so a
/// zero clock asks for an infinite window instead of erroring, and on Renode
/// [`check_clock_declarations`] holds the platform's own declarations to it.
fn validate_frequency(frequency_hz: u64) -> Result<(), SocError> {
    if frequency_hz == 0 {
        return Err(SocError::ZeroFrequency);
    }
    Ok(())
}

/// Read one `.repl` property's value wherever it appears in a platform source.
///
/// The grammar this needs is tiny: `.repl` writes `name: value` one per line,
/// values here are plain decimal or `0x` integers, and `//` starts a comment.
/// A full parser would be a second implementation of Renode's, which is exactly
/// the kind of thing that drifts; this reads the two properties that describe a
/// core clock and ignores everything else. Comment lines are skipped so the
/// prose in a descriptor (which quotes the old wrong values on purpose, as
/// evidence) is not mistaken for a declaration.
#[cfg(feature = "renode")]
fn repl_property_values(source: &str, property: &str) -> Vec<u64> {
    let mut out = Vec::new();
    for line in source.lines() {
        let line = line.trim();
        if line.starts_with("//") || line.starts_with('#') {
            continue;
        }
        // Strip a trailing comment before looking for the property, so
        // `frequency: 40000  // the IWDG's own clock` still parses.
        let code = line.split("//").next().unwrap_or(line);
        let Some(rest) = code.split_once(property).map(|(_, r)| r) else {
            continue;
        };
        let Some(value) = rest.strip_prefix(':') else {
            continue;
        };
        let value = value.trim().trim_end_matches(';').trim();
        let parsed = match value
            .strip_prefix("0x")
            .or_else(|| value.strip_prefix("0X"))
        {
            Some(hex) => u64::from_str_radix(hex, 16).ok(),
            None => value.parse::<u64>().ok(),
        };
        if let Some(v) = parsed {
            out.push(v);
        }
    }
    out
}

/// Cross-check the platform's declared CORE clock against `soc.frequency_hz`,
/// and refuse a part that declares none.
///
/// # Why this is a load failure and not a lint
///
/// `frequency_hz` was decorative on Renode: it cancels out of both
/// `cycles = seconds * frequency_hz` and `Mcu::frequency`, so a descriptor could
/// claim an 8 MHz part while its platform ran a 72 MHz SysTick and nothing
/// anywhere disagreed. That is what let four backends ship running 4.5x to 9x
/// fast. A warning would have been ignored for the same reason the mismatch was:
/// nothing downstream depended on the number. Refusing at load makes the two
/// declarations one fact with one failure mode, so a new part cannot be added
/// with a lying clock even by an author who has never read this file.
///
/// # What counts as the core clock
///
/// Only `cpu PerformanceInMips` (the instructions-per-second knob, whose honest
/// value is the core clock in MHz, as `db/mcu/rp2040/rp2040.repl` already had
/// it) and `nvic systickFrequency` (the Cortex-M SysTick source). Every OTHER
/// `frequency:` in a platform is a different clock domain, a 40 kHz IWDG, a
/// 32768 Hz WDT, a timer block on its own APB branch, and holding those to the
/// core rate would be wrong. Those domains are documented per descriptor and
/// gated by measurement (`tests/clock_truth.rs`) rather than by this check.
///
/// A part with no NVIC (the RISC-V FE310) legitimately declares no
/// `systickFrequency`; `PerformanceInMips` is the one declaration every Renode
/// part can and must make.
#[cfg(feature = "renode")]
fn check_clock_declarations(
    platform_repl: &str,
    support_bundle: Option<&str>,
    frequency_hz: u64,
) -> Result<(), SocError> {
    // Every source the declarations could live in. Inline `platform_repl` is
    // the normal case; a bundled platform's `.repl` files are embedded in the
    // binary, so a part whose platform lives in a bundle is checked too rather
    // than exempted (RP2040 is the one backend that was already correct, and it
    // should be able to prove that, not be trusted about it).
    let mut sources: Vec<String> = Vec::new();
    if platform_repl.contains('\n') {
        sources.push(platform_repl.to_string());
    }
    if let Some(bundle) = support_bundle.and_then(crate::renode::support::lookup) {
        sources.extend(bundle.repl_sources());
    }

    let mut found_any = false;
    for source in &sources {
        for declared in repl_property_values(source, "systickFrequency") {
            found_any = true;
            if declared != frequency_hz {
                return Err(SocError::ClockMismatch {
                    property: "nvic systickFrequency",
                    declared,
                    frequency_hz,
                    expected_prose: format!("not the declared part clock of {frequency_hz} Hz"),
                    ratio: declared as f64 / frequency_hz as f64,
                });
            }
        }
        for declared in repl_property_values(source, "PerformanceInMips") {
            found_any = true;
            // MIPS is the core clock in MHz, so a part whose clock is not a
            // whole number of MHz cannot be expressed and the mismatch below
            // reports it rather than silently rounding.
            if declared.saturating_mul(1_000_000) != frequency_hz {
                return Err(SocError::ClockMismatch {
                    property: "cpu PerformanceInMips",
                    declared,
                    frequency_hz,
                    expected_prose: format!(
                        "not the declared part clock of {frequency_hz} Hz expressed in MHz \
                         ({} MIPS)",
                        frequency_hz / 1_000_000
                    ),
                    ratio: (declared as f64 * 1e6) / frequency_hz as f64,
                });
            }
        }
    }

    if !found_any {
        return Err(SocError::UndeclaredClock {
            platform: platform_repl.trim().trim_start_matches('@').to_string(),
            mips: frequency_hz / 1_000_000,
            frequency_hz,
        });
    }
    Ok(())
}

/// Refuse an identifier field that is present but blank.
///
/// `platform_repl` has its own [`SocError::EmptyPlatform`] because Renode has
/// nothing to load without it. These are the fields that instead reach the
/// emulator as an empty argument, where the failure arrives seconds later and
/// names the emulator rather than the descriptor. `mcu_label` is deliberately
/// NOT here: it appears only in reports and error messages, so a blank one runs
/// correctly and is a `models lint` finding rather than a load failure.
fn validate_non_empty(fields: &[(&'static str, &str)]) -> Result<(), SocError> {
    for (field, value) in fields {
        if value.trim().is_empty() {
            return Err(SocError::EmptyField { field });
        }
    }
    Ok(())
}

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
    (
        "renode:stm32f072",
        include_str!("../db/mcu/stm32f072.soc.toml"),
    ),
    (
        "renode:stm32f103",
        include_str!("../db/mcu/stm32f103.soc.toml"),
    ),
    (
        "renode:stm32f4_discovery",
        include_str!("../db/mcu/stm32f4_discovery.soc.toml"),
    ),
    (
        "renode:nrf52840",
        include_str!("../db/mcu/nrf52840.soc.toml"),
    ),
    (
        "renode:sifive_fe310",
        include_str!("../db/mcu/sifive_fe310.soc.toml"),
    ),
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
                    Ok(SocConfig::Renode(
                        crate::renode::RenodeConfig::from_soc_toml(src)?,
                    ))
                }
                #[cfg(not(feature = "renode"))]
                {
                    Err(SocError::BackendDisabled("renode".to_string()))
                }
            }
            Backend::Qemu => {
                #[cfg(feature = "qemu")]
                {
                    Ok(SocConfig::Qemu(crate::qemu::QemuConfig::from_soc_toml(
                        src,
                    )?))
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
                        });
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

    /// The embedded TOML SOURCE for a `backend:part` spec.
    ///
    /// [`builtin_specs`](Self::builtin_specs) says which parts exist and
    /// [`resolve`](Self::resolve) hands back a parsed config; neither gives a
    /// caller the file. Anything that reads a descriptor AS A FILE needs this:
    /// `hauksbee models lint`'s sweep over every shipped descriptor (so a broken
    /// one cannot ship), and any command that shows or copies a built-in as the
    /// starting point for a new part.
    ///
    /// Note the layering: this is the EMBEDDED source, deliberately not the
    /// override-directory file that `resolve` would prefer. A caller asking for
    /// the shipped descriptor is asking about the binary, not the machine.
    pub fn builtin_source(spec: &str) -> Option<&'static str> {
        EMBEDDED.iter().find(|(k, _)| *k == spec).map(|(_, v)| *v)
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
         or ~/.config/hauksbee/mcu ({}).",
        builtins.join(", "),
        hauksbee_ir::docs_url("docs/extending/add-an-mcu-variant.md")
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
            assert!(
                hint.contains(b),
                "hint must list every built-in ({b}): {hint}"
            );
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
        let tmp =
            std::env::temp_dir().join(format!("hauksbee-soc-test-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let w = env_dir_missing_warning(&tmp).expect("a missing dir must warn");
        assert!(
            w.contains("HAUKSBEE_MCU_DIR") && w.contains("does not exist"),
            "{w}"
        );
        std::fs::create_dir_all(&tmp).unwrap();
        assert!(
            env_dir_missing_warning(&tmp).is_none(),
            "an existing dir must not warn"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // U3 finding 5: falling through to a built-in while the override dir holds
    // OTHER descriptors hints at the exact expected filename.
    #[test]
    fn builtin_fallback_names_the_expected_descriptor_filename() {
        let tmp =
            std::env::temp_dir().join(format!("hauksbee-soc-test-fallback-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("stm32f103-mine.soc.toml"), "x").unwrap();
        let hints = builtin_fallback_hints("stm32f103", &[tmp.clone()]);
        assert_eq!(hints.len(), 1, "{hints:?}");
        assert!(
            hints[0].contains("stm32f103.soc.toml") && hints[0].contains("stm32f103-mine.soc.toml"),
            "hint must name both the expected filename and what IS there: {}",
            hints[0]
        );
        // An empty dir produces no hint (nothing there was meant to override).
        let empty = tmp.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(builtin_fallback_hints("stm32f103", &[empty]).is_empty());
        // A missing dir produces no hint either.
        assert!(builtin_fallback_hints("stm32f103", &[tmp.join("no-such-subdir")]).is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}

/// The loader's refusals for descriptors that would run and observe the wrong
/// thing, or run without bound.
///
/// Each case here is a value the schema accepted until it was checked, and each
/// one is refused rather than merely warned about because none of them has a
/// correct execution: the checks that depend on what an author *meant* live in
/// `hauksbee models lint` instead.
#[cfg(all(test, feature = "renode"))]
mod loader_refusal_tests {
    use super::{SocConfig, SocError};

    /// A minimal valid Renode descriptor, with `{extra}` splicing in whatever
    /// the case under test is changing.
    fn descriptor(extra: &str) -> String {
        format!(
            r#"
[soc]
backend = "renode"
machine = "m"
platform_repl = """
using "platforms/cpus/stm32f072.repl"

nvic:
    systickFrequency: 8000000

cpu:
    PerformanceInMips: 8
"""
cpu_path = "sysbus.cpu"
frequency_hz = 8_000_000
expected_e_machine = "EM_ARM"
mcu_label = "test part"
{extra}
"#
        )
    }

    fn err(extra: &str) -> SocError {
        SocConfig::from_soc_toml(&descriptor(extra)).expect_err("this descriptor must be refused")
    }

    #[test]
    fn the_baseline_descriptor_is_valid() {
        SocConfig::from_soc_toml(&descriptor("")).expect("the baseline must load");
    }

    /// The whole point of the clock cross-check: a descriptor that claims one
    /// clock while its platform declares another is REFUSED, not warned about.
    /// The two numbers here are the real ones the STM32F103 shipped with, and
    /// the 9.00x the error reports is the rate the clock-truth gate measured.
    #[test]
    fn a_platform_clock_that_disagrees_with_frequency_hz_is_refused() {
        let lying =
            descriptor("").replace("systickFrequency: 8000000", "systickFrequency: 72000000");
        match SocConfig::from_soc_toml(&lying) {
            Err(SocError::ClockMismatch {
                property,
                declared,
                frequency_hz,
                ratio,
                ..
            }) => {
                assert_eq!(property, "nvic systickFrequency");
                assert_eq!(declared, 72_000_000);
                assert_eq!(frequency_hz, 8_000_000);
                assert!((ratio - 9.0).abs() < 1e-9, "ratio was {ratio}");
            }
            other => panic!("a 9x clock lie must be refused at load, got {other:?}"),
        }

        // And the CPU-speed half, which is the other measured error: 100 MIPS
        // against an 8 MHz part is the 7.14x an instruction busy-wait showed.
        let lying = descriptor("").replace("PerformanceInMips: 8", "PerformanceInMips: 100");
        match SocConfig::from_soc_toml(&lying) {
            Err(SocError::ClockMismatch {
                property, declared, ..
            }) => {
                assert_eq!(property, "cpu PerformanceInMips");
                assert_eq!(declared, 100);
            }
            other => {
                panic!("a 100-MIPS declaration on an 8 MHz part must be refused, got {other:?}")
            }
        }
    }

    /// A platform that declares NO clock is refused too, because that is how
    /// the nRF52840 came to run 6.58x fast: the stock file declared neither
    /// property, so both fell to Renode defaults unrelated to the part.
    #[test]
    fn a_platform_that_declares_no_clock_at_all_is_refused() {
        let silent = format!(
            r#"
[soc]
backend = "renode"
machine = "m"
platform_repl = "@platforms/cpus/stm32f072.repl"
cpu_path = "sysbus.cpu"
frequency_hz = 8_000_000
expected_e_machine = "EM_ARM"
mcu_label = "test part"
"#
        );
        match SocConfig::from_soc_toml(&silent) {
            Err(SocError::UndeclaredClock { mips, platform, .. }) => {
                assert_eq!(mips, 8);
                // The message quotes the path back so the author can paste the
                // `using` line rather than work it out.
                assert_eq!(platform, "platforms/cpus/stm32f072.repl");
            }
            other => panic!("a platform with no declared clock must be refused, got {other:?}"),
        }
    }

    /// Other clock DOMAINS are left alone. A 40 kHz watchdog and a 32768 Hz RTC
    /// are not the core clock, and a check that policed every `frequency:` in a
    /// platform would refuse every correct descriptor in the tree.
    #[test]
    fn a_non_core_clock_domain_is_not_held_to_the_core_rate() {
        let with_iwdg = descriptor("").replace(
            "cpu:\n    PerformanceInMips: 8",
            "iwdg: Timers.STM32_IndependentWatchdog @ sysbus 0x40003000\n    frequency: 40000\n\ncpu:\n    PerformanceInMips: 8",
        );
        SocConfig::from_soc_toml(&with_iwdg)
            .expect("a 40 kHz watchdog is a different clock domain, not a lying core clock");
    }

    /// The prose in a descriptor quotes the OLD WRONG values on purpose, as the
    /// evidence for why the fix exists. A check that read comments would refuse
    /// every descriptor that documents itself.
    #[test]
    fn a_commented_out_declaration_is_not_a_declaration() {
        let documented = descriptor("").replace(
            "nvic:",
            "// was `systickFrequency: 72000000`, which ran 9.00x fast\nnvic:",
        );
        SocConfig::from_soc_toml(&documented)
            .expect("a comment quoting the old wrong value is evidence, not a declaration");
    }

    /// Every shipped descriptor passes the cross-check, which is the claim that
    /// makes the clock-truth gate's per-part numbers meaningful: the gate
    /// measures three parts, this covers all of them.
    #[test]
    fn every_builtin_descriptor_declares_a_consistent_clock() {
        for spec in SocConfig::builtin_specs() {
            let src = SocConfig::builtin_source(spec).expect("advertised spec has source");
            SocConfig::from_soc_toml(src).unwrap_or_else(|e| {
                panic!("shipped descriptor {spec} must pass the clock cross-check: {e}")
            });
        }
    }

    /// Every spec the resolver advertises has embedded source behind it, and
    /// that source is the descriptor the resolver loads.
    #[test]
    fn every_builtin_spec_has_source_and_an_unknown_one_has_none() {
        for spec in SocConfig::builtin_specs() {
            let src = SocConfig::builtin_source(spec)
                .unwrap_or_else(|| panic!("{spec} is advertised but carries no source"));
            assert!(
                src.contains("[soc]"),
                "{spec}'s source must be a descriptor"
            );
            SocConfig::from_soc_toml(src)
                .unwrap_or_else(|e| panic!("shipped descriptor {spec} must load: {e}"));
        }
        assert_eq!(SocConfig::builtin_source("renode:no_such_part"), None);
    }

    #[test]
    fn zero_frequency_is_refused() {
        let src = descriptor("").replace("frequency_hz = 8_000_000", "frequency_hz = 0");
        let e = SocConfig::from_soc_toml(&src).expect_err("a zero clock must be refused");
        assert!(matches!(e, SocError::ZeroFrequency), "{e}");
        assert!(e.to_string().contains("run window"), "{e}");
    }

    #[test]
    fn blank_machine_and_cpu_path_are_refused_but_a_blank_label_is_not() {
        let blank_machine = descriptor("").replace("machine = \"m\"", "machine = \"  \"");
        assert!(
            matches!(
                SocConfig::from_soc_toml(&blank_machine),
                Err(SocError::EmptyField { field: "machine" })
            ),
            "a blank machine name must be refused"
        );
        let blank_cpu = descriptor("").replace("cpu_path = \"sysbus.cpu\"", "cpu_path = \"\"");
        assert!(
            matches!(
                SocConfig::from_soc_toml(&blank_cpu),
                Err(SocError::EmptyField { field: "cpu_path" })
            ),
            "a blank cpu path must be refused"
        );
        assert!(
            matches!(
                SocConfig::from_soc_toml(&descriptor("uart = \"\"")),
                Err(SocError::EmptyField { field: "uart" })
            ),
            "a blank UART path must be refused; omitting the field is how a \
             descriptor asks for no bridge"
        );
        // Omitting it is the legitimate way to say that.
        SocConfig::from_soc_toml(&descriptor("")).expect("no `uart` field means no UART bridge");
        // `mcu_label` reaches reports and error messages, never the emulator, so
        // it LOADS: `models lint` is where a blank one is reported.
        let blank_label = descriptor("").replace("mcu_label = \"test part\"", "mcu_label = \"\"");
        SocConfig::from_soc_toml(&blank_label)
            .expect("a blank label runs correctly and is a lint finding, not a load failure");
    }

    #[test]
    fn a_support_token_with_no_bundle_is_refused_per_field() {
        let platform = format!(
            r#"
[soc]
backend = "renode"
machine = "m"
platform_repl = "@{{support}}/mine.repl"
cpu_path = "sysbus.cpu"
frequency_hz = 8_000_000
expected_e_machine = "EM_ARM"
mcu_label = "test part"
"#
        );
        assert!(
            matches!(
                SocConfig::from_soc_toml(&platform),
                Err(SocError::SupportTokenWithoutBundle {
                    field: "platform_repl"
                })
            ),
            "an unsubstitutable platform token must be refused"
        );
        assert!(
            matches!(
                err(r#"extra_setup = ["sysbus LoadELF @{support}/bootrom.elf"]"#),
                SocError::SupportTokenWithoutBundle {
                    field: "extra_setup"
                }
            ),
            "an unsubstitutable extra_setup token must be refused"
        );
        assert!(
            matches!(
                err(r#"post_load_setup = ["include @{support}/late.cs"]"#),
                SocError::SupportTokenWithoutBundle {
                    field: "post_load_setup"
                }
            ),
            "an unsubstitutable post_load_setup token must be refused"
        );
    }

    #[test]
    fn a_16_pin_dir_encoding_on_a_wider_port_is_refused() {
        let e = err(
            "[[soc.ports]]\nletter = \"0\"\nperipheral = \"sio\"\nodr_offset = 0x10\n\
             width = 32\ndir = { offset = 0x20, encoding = \"moder\" }",
        );
        match e {
            SocError::DirEncodingTooNarrow {
                letter: '0',
                width: 32,
                encoding: "moder",
            } => {}
            other => panic!("expected DirEncodingTooNarrow, got {other}"),
        }
        // `dir_bits` reaches all 32, so the same port with that encoding loads.
        SocConfig::from_soc_toml(&descriptor(
            "[[soc.ports]]\nletter = \"0\"\nperipheral = \"sio\"\nodr_offset = 0x10\n\
             width = 32\ndir = { offset = 0x20, encoding = \"dir_bits\" }",
        ))
        .expect("dir_bits covers 32 pins");
        // And a 16-bit port with `moder` is the ordinary STM32 case.
        SocConfig::from_soc_toml(&descriptor(
            "[[soc.ports]]\nletter = \"C\"\nperipheral = \"gpioPortC\"\nodr_offset = 0x14\n\
             width = 16\ndir = { offset = 0x00, encoding = \"moder\" }",
        ))
        .expect("moder covers 16 pins");
    }

    #[test]
    fn a_duplicated_adc_channel_is_refused() {
        let e = err(
            "[[soc.adc]]\nchannel = 3\nmonitor_command = \"a {count}\"\n\
             full_scale_volts = 3.3\nmax_count = 4095\n\
             [[soc.adc]]\nchannel = 3\nmonitor_command = \"b {count}\"\n\
             full_scale_volts = 3.3\nmax_count = 4095",
        );
        assert!(
            matches!(e, SocError::DuplicateAdcChannel { channel: 3 }),
            "{e}"
        );
    }

    #[test]
    fn an_adc_scale_that_cannot_scale_is_refused() {
        let zero_count = err(
            "[[soc.adc]]\nchannel = 0\nmonitor_command = \"a {count}\"\n\
             full_scale_volts = 3.3\nmax_count = 0",
        );
        assert!(
            matches!(
                zero_count,
                SocError::AdcScaleInvalid {
                    channel: 0,
                    field: "max_count",
                    ..
                }
            ),
            "{zero_count}"
        );
        for full_scale in ["0.0", "-3.3", "nan", "inf"] {
            let e = err(&format!(
                "[[soc.adc]]\nchannel = 0\nmonitor_command = \"a {{count}}\"\n\
                 full_scale_volts = {full_scale}\nmax_count = 4095"
            ));
            assert!(
                matches!(
                    e,
                    SocError::AdcScaleInvalid {
                        channel: 0,
                        field: "full_scale_volts",
                        ..
                    }
                ),
                "full_scale_volts = {full_scale} must be refused, got {e}"
            );
        }
    }

    /// A `monitor_command` with no substitution token LOADS. It executes, and
    /// feeds one constant every chunk, which is a thing an author can mean (a
    /// self-contained trigger) and `models lint` reports as a finding. The
    /// loader draws its line at descriptors with no correct execution.
    #[test]
    fn a_tokenless_monitor_command_loads_and_is_left_to_the_linter() {
        SocConfig::from_soc_toml(&descriptor(
            "[[soc.adc]]\nchannel = 0\nmonitor_command = \"sysbus.adc SetDefaultValue 1650\"\n\
             full_scale_volts = 3.3\nmax_count = 4095",
        ))
        .expect("a tokenless feed executes, so the loader accepts it");
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
        assert!(matches!(
            err,
            SocError::PortTooWide {
                letter: 'A',
                width: 40
            }
        ));
        // 32 is the maximum legal width and must pass.
        assert!(validate_ports([('A', 32)].into_iter()).is_ok());
        // A normal 16-bit port still validates.
        assert!(validate_ports([('B', 16)].into_iter()).is_ok());
    }
}
