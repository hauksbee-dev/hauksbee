//! Platform-support bundles: peripheral models Renode does not ship.
//!
//! # Why this exists
//!
//! The STM32F1 precedent (`db/mcu/stm32f103.soc.toml`) folds a hauksbee-authored
//! `.repl` into the descriptor so stock CubeMX firmware boots. That works because
//! Renode already *has* an STM32F103 platform and the fix is an extension of it.
//!
//! RP2040 has no such starting point. Renode 1.16.1 ships no rp2040 platform and
//! neither does Renode `master`, so there is no SIO, no RP2040 clock tree, no
//! RP2040 timer, no PL011-with-DREQ UART: the *models* are missing, not just
//! their wiring. A `.repl` cannot conjure a peripheral class that the emulator
//! has never compiled.
//!
//! Renode can compile C# at run time, though: `include <file.cs>` on the Monitor
//! drives its bundled compiler and registers the resulting peripheral types, and
//! the backend already relies on that for the I2C/SPI bridge peripherals. A
//! bundle is that mechanism scaled up to a whole SoC: a set of `.cs` peripheral
//! models plus the data files the platform reads (an SVD, a boot ROM image),
//! embedded in this binary, unpacked to a temp directory, and `include`d before
//! the platform description parses.
//!
//! # The contract
//!
//! A descriptor opts in with `[soc] support_bundle = "<name>"`. At machine
//! bring-up, before `machine LoadPlatformDescription`, the backend:
//!
//!   1. unpacks the bundle into a fresh temp directory;
//!   2. runs `path add <dir>` so bare `@name` references inside the bundle's own
//!      `.repl` (its `ApplySVD`) resolve without any path rewriting;
//!   3. runs `include <dir>/peripherals/<file>.cs` for every source **in the
//!      declared order**, because Renode's C# include is order sensitive: a
//!      later file referencing an earlier file's type fails to compile if the
//!      order is wrong.
//!
//! Afterwards the literal `{support}` token in the descriptor's `platform_repl`,
//! `extra_setup` and `post_load_setup` is substituted with the unpacked
//! directory, so the descriptor stays readable (`@{support}/rp2040.repl`) while
//! the paths Renode sees are absolute.
//!
//! The directory is removed when the backend drops, and it is per-process and
//! content-addressed so parallel test binaries never share or race one.
//!
//! # Why unpack rather than reference the source tree
//!
//! Because an installed `hauksbee` is one binary with no repository beside it.
//! The shipped `db/mcu/*.soc.toml` descriptors are already `include_str!`-ed for
//! exactly that reason; a bundle is the same decision applied to files that must
//! exist on disk because Renode, not hauksbee, is the one reading them.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// One file inside a bundle.
struct BundleFile {
    /// Path relative to the unpacked bundle root, e.g. `peripherals/rp2040_sio.cs`.
    name: &'static str,
    bytes: &'static [u8],
}

/// A named set of files plus the order to `include` the C# sources in.
pub struct SupportBundle {
    /// Descriptor-facing name (`[soc] support_bundle = "rp2040"`).
    pub name: &'static str,
    files: &'static [BundleFile],
    /// C# sources to `include`, in dependency order. Each entry is a `name` of
    /// one of `files`.
    sources: &'static [&'static str],
}

/// Look up a bundle by its descriptor name.
pub fn lookup(name: &str) -> Option<&'static SupportBundle> {
    BUNDLES.iter().copied().find(|b| b.name == name)
}

/// Every bundle name this build knows, for error messages and validation.
pub fn known_names() -> Vec<&'static str> {
    BUNDLES.iter().map(|b| b.name).collect()
}

static BUNDLES: &[&SupportBundle] = &[&RP2040];

impl SupportBundle {
    /// Write every file to a fresh directory under the system temp dir and
    /// return the directory.
    ///
    /// The name carries the process id so two hauksbee processes never collide,
    /// and a counter so two machines inside one process (the co-sim test suite
    /// runs several) get their own copy rather than one racing the other's
    /// cleanup.
    pub fn unpack(&self) -> Result<PathBuf> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "hauksbee-renode-support-{}-{}-{}",
            self.name,
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        for file in self.files {
            let path = dir.join(file.name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            std::fs::write(&path, file.bytes)
                .with_context(|| format!("unpacking {}", path.display()))?;
        }
        Ok(dir)
    }

    /// The Monitor commands that register this bundle's peripheral types, in
    /// order, for a bundle already unpacked at `dir`.
    ///
    /// `path add` comes first so the bundle's own `.repl` can name its data
    /// files relatively.
    pub fn prelude_commands(&self, dir: &Path) -> Vec<String> {
        let mut cmds = Vec::with_capacity(self.sources.len() + 1);
        cmds.push(format!("path add @{}", dir.display()));
        for src in self.sources {
            cmds.push(format!("include @{}", dir.join(src).display()));
        }
        cmds
    }
}

/// Embed one file of the RP2040 bundle, keyed on its path relative to the
/// bundle root so the `files` list below reads as the on-disk layout.
macro_rules! rp2040_file {
    ($rel:literal) => {
        BundleFile {
            name: $rel,
            bytes: include_bytes!(concat!("../../db/mcu/rp2040/", $rel)),
        }
    };
}

/// The RP2040 bundle. Provenance and licences: `db/mcu/rp2040/README.md`.
///
/// `sources` is the load order, copied from upstream's
/// `cores/initialize_peripherals_source.resc` with the entries hauksbee does not
/// use dropped (the segment-display / BMP280 / PCF8523 demo externals). Two
/// entries look droppable and are not:
///
///   - `rp2040_pio.cs` is never instantiated (see the README on PIO) but the
///     SIO, GPIO, SPI and ADC models reference its types, so omitting it is a
///     compile error, not a smaller platform.
///   - `w25q16.cs` provides `SPI.W25QXX`, which the platform attaches to the XIP
///     SSI. Nothing in the proven path reads flash over QSPI, but the platform
///     declares the chip, so the type must exist for the description to parse.
static RP2040: SupportBundle = SupportBundle {
    name: "rp2040",
    files: &[
        rp2040_file!("rp2040.repl"),
        rp2040_file!("RP2040.svd.gz"),
        rp2040_file!("bootrom.elf"),
        rp2040_file!("peripherals/w25q16.cs"),
        rp2040_file!("peripherals/memory_alias.cs"),
        rp2040_file!("peripherals/rp2040_peripheral_base.cs"),
        rp2040_file!("peripherals/rp2040_xosc.cs"),
        rp2040_file!("peripherals/rp2040_rosc.cs"),
        rp2040_file!("peripherals/rp2040_pll.cs"),
        rp2040_file!("peripherals/power.cs"),
        rp2040_file!("peripherals/rp2040_gpio.cs"),
        rp2040_file!("peripherals/rp2040_clocks.cs"),
        rp2040_file!("peripherals/rp2040_timer.cs"),
        rp2040_file!("peripherals/rp2040_pads.cs"),
        rp2040_file!("peripherals/rp2040_qspi_pads.cs"),
        rp2040_file!("peripherals/rp2040_pio.cs"),
        rp2040_file!("peripherals/rp2040_spi.cs"),
        rp2040_file!("peripherals/rp2040_xip_ssi.cs"),
        rp2040_file!("peripherals/rp2040_sio.cs"),
        rp2040_file!("peripherals/rp2040_adc.cs"),
        rp2040_file!("peripherals/rp2040_uart.cs"),
        rp2040_file!("peripherals/rpdma_engine.cs"),
        rp2040_file!("peripherals/rpdma.cs"),
        rp2040_file!("peripherals/rp2040_watchdog.cs"),
        rp2040_file!("peripherals/rp2040_i2c.cs"),
        rp2040_file!("peripherals/rp2040_psm.cs"),
    ],
    sources: &[
        "peripherals/w25q16.cs",
        "peripherals/memory_alias.cs",
        "peripherals/rp2040_peripheral_base.cs",
        "peripherals/rp2040_xosc.cs",
        "peripherals/rp2040_rosc.cs",
        "peripherals/rp2040_pll.cs",
        "peripherals/power.cs",
        "peripherals/rp2040_gpio.cs",
        "peripherals/rp2040_clocks.cs",
        "peripherals/rp2040_timer.cs",
        "peripherals/rp2040_pads.cs",
        "peripherals/rp2040_qspi_pads.cs",
        "peripherals/rp2040_pio.cs",
        "peripherals/rp2040_spi.cs",
        "peripherals/rp2040_xip_ssi.cs",
        "peripherals/rp2040_sio.cs",
        "peripherals/rp2040_adc.cs",
        "peripherals/rp2040_uart.cs",
        "peripherals/rpdma_engine.cs",
        "peripherals/rpdma.cs",
        "peripherals/rp2040_watchdog.cs",
        "peripherals/rp2040_i2c.cs",
        "peripherals/rp2040_psm.cs",
    ],
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `sources` entry must name a real `files` entry, and every `.cs`
    /// file carried must be in `sources`. A source listed but not carried
    /// unpacks to a missing file and fails at machine bring-up with a Renode
    /// error; a `.cs` carried but not listed is dead weight in the binary that
    /// silently does nothing, which is how a peripheral goes missing without
    /// anyone noticing.
    #[test]
    fn bundle_sources_and_files_agree() {
        for bundle in BUNDLES {
            let carried: Vec<&str> = bundle.files.iter().map(|f| f.name).collect();
            for src in bundle.sources {
                assert!(
                    carried.contains(src),
                    "{}: source {src} is not carried in files",
                    bundle.name
                );
            }
            for name in &carried {
                if name.ends_with(".cs") {
                    assert!(
                        bundle.sources.contains(name),
                        "{}: {name} is carried but never included, so its \
                         peripheral types are never registered",
                        bundle.name
                    );
                }
            }
        }
    }

    /// No embedded file may be empty: an `include_bytes!` of a path that exists
    /// but was truncated (a bad vendor refresh) would otherwise surface as an
    /// opaque C# compile error inside Renode.
    #[test]
    fn bundle_files_are_non_empty() {
        for bundle in BUNDLES {
            for file in bundle.files {
                assert!(
                    !file.bytes.is_empty(),
                    "{}: {} is embedded but empty",
                    bundle.name,
                    file.name
                );
            }
        }
    }

    #[test]
    fn lookup_finds_rp2040_and_rejects_unknown() {
        assert!(lookup("rp2040").is_some());
        assert!(lookup("rp2041").is_none());
        assert_eq!(known_names(), vec!["rp2040"]);
    }

    #[test]
    fn unpack_writes_every_file_and_prelude_points_at_them() {
        let bundle = lookup("rp2040").unwrap();
        let dir = bundle.unpack().expect("unpack rp2040 bundle");
        for file in bundle.files {
            let path = dir.join(file.name);
            assert!(path.exists(), "{} was not unpacked", path.display());
        }
        let cmds = bundle.prelude_commands(&dir);
        assert_eq!(cmds.len(), bundle.sources.len() + 1);
        assert!(cmds[0].starts_with("path add @"));
        // Every include must name a file that is now on disk.
        for cmd in &cmds[1..] {
            let p = cmd.trim_start_matches("include @");
            assert!(Path::new(p).exists(), "include target missing: {p}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Two unpacks inside one process must not share a directory: the backend
    /// deletes its own on drop, and a shared directory would pull the rug from
    /// under a still-running machine.
    #[test]
    fn unpack_is_unique_per_call() {
        let bundle = lookup("rp2040").unwrap();
        let a = bundle.unpack().unwrap();
        let b = bundle.unpack().unwrap();
        assert_ne!(a, b);
        std::fs::remove_dir_all(&a).ok();
        std::fs::remove_dir_all(&b).ok();
    }
}
