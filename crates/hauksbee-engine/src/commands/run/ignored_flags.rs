//! Says aloud when a flag the user typed is one the chosen surface does not
//! use. `run` picks one surface from its flags, and a flag that belongs to a
//! different surface used to vanish without a word (`--lint --seconds 5` ran
//! the lint and no simulation, with nothing to say so). Which flags a surface
//! uses is this table, one row per flag, so a new surface or flag is one edit
//! and no surface needs its own special case.

use hauksbee_ir::eprintln;

use super::{RunConfig, SelectedSurface};

/// A flag that only some surfaces use, and the surfaces that do.
struct SurfaceFlag {
    /// The flag as the user types it.
    flag: &'static str,
    /// What the flag sets, for the sentence that says it was not used.
    what: &'static str,
    /// The surfaces that read it; every other surface drops it.
    used_by: &'static [SelectedSurface],
}

/// Flags that mean something only to a simulating surface. A flag a surface
/// reads belongs on its row; a surface not on a row never sees that flag.
const SURFACE_FLAGS: &[SurfaceFlag] = &[
    SurfaceFlag {
        flag: "--seconds",
        what: "simulated time",
        used_by: &[
            SelectedSurface::Headless,
            SelectedSurface::Thermal,
            SelectedSurface::Serial,
            SelectedSurface::CheckThenCosim,
        ],
    },
    SurfaceFlag {
        flag: "--chunk-us",
        what: "the solver chunk width",
        used_by: &[
            SelectedSurface::Headless,
            SelectedSurface::Serial,
            SelectedSurface::CheckThenCosim,
        ],
    },
    SurfaceFlag {
        flag: "--vcd",
        what: "the VCD waveform file",
        used_by: &[SelectedSurface::Headless, SelectedSurface::CheckThenCosim],
    },
    SurfaceFlag {
        flag: "--bus-log",
        what: "the bus log",
        used_by: &[SelectedSurface::Headless, SelectedSurface::CheckThenCosim],
    },
    SurfaceFlag {
        flag: "--firmware",
        what: "the firmware image",
        used_by: &[
            SelectedSurface::Headless,
            SelectedSurface::Thermal,
            SelectedSurface::Serial,
            SelectedSurface::CheckThenCosim,
            SelectedSurface::Default,
            // The static suite reads its interrupt vectors (see
            // `reports::firmware_intent`).
            SelectedSurface::Check,
            SelectedSurface::Lint,
            SelectedSurface::BareJson,
        ],
    },
];

/// How the user names a surface: the flag that selects it.
const fn label(surface: SelectedSurface) -> &'static str {
    match surface {
        SelectedSurface::Inventory => "--list-nets",
        SelectedSurface::Netlist => "--emit-netlist",
        SelectedSurface::Check => "--check",
        SelectedSurface::Bind => "--report",
        SelectedSurface::Drc => "--drc",
        SelectedSurface::Ampacity => "--ampacity",
        SelectedSurface::Lint => "--lint",
        SelectedSurface::Resources => "--resources",
        SelectedSurface::UsbC => "--usb-c",
        SelectedSurface::Si => "--si",
        SelectedSurface::Ac => "--ac",
        SelectedSurface::Thermal => "--thermal",
        SelectedSurface::Serial => "--serial-attach",
        SelectedSurface::Headless => "--headless",
        SelectedSurface::CheckThenCosim => "bare --plain with --seconds",
        SelectedSurface::BareJson => "--json",
        SelectedSurface::Default => "--serve",
    }
}

/// The flags the user typed that `surface` does not use, one sentence each.
fn unused(typed: &[String], surface: SelectedSurface) -> Vec<String> {
    SURFACE_FLAGS
        .iter()
        .filter(|row| typed.iter().any(|t| t == row.flag) && !row.used_by.contains(&surface))
        .map(|row| {
            let wanted: Vec<&str> = row.used_by.iter().map(|s| label(*s)).collect();
            format!(
                "warning: {} ({}) was not used: {} does not read it. It applies to {}.",
                row.flag,
                row.what,
                label(surface),
                wanted.join(", ")
            )
        })
        .collect()
}

/// Print one line per typed flag the chosen surface drops, on stderr, so a
/// report on stdout stays clean and nothing the user asked for goes unsaid.
pub(super) fn warn(cfg: &RunConfig, surface: SelectedSurface) {
    for line in unused(&cfg.explicit_flags, surface) {
        eprintln!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::{unused, SelectedSurface, SURFACE_FLAGS};

    fn passed(flags: &[&str]) -> Vec<String> {
        flags.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn a_typed_flag_a_static_report_drops_is_named_with_where_it_applies() {
        let lines = unused(&passed(&["--seconds"]), SelectedSurface::Lint);
        assert_eq!(lines.len(), 1, "{lines:?}");
        let line = &lines[0];
        assert!(
            line.contains("--seconds")
                && line.contains("was not used")
                && line.contains("--lint")
                && line.contains("--headless")
                && line.contains("bare --plain with --seconds"),
            "{line}"
        );
    }

    #[test]
    fn a_flag_the_surface_reads_or_the_user_did_not_type_is_not_flagged() {
        assert!(unused(&passed(&["--seconds"]), SelectedSurface::Headless).is_empty());
        assert!(unused(&passed(&["--seconds"]), SelectedSurface::CheckThenCosim).is_empty());
        assert!(unused(&passed(&[]), SelectedSurface::Lint).is_empty());
    }

    #[test]
    fn every_row_names_a_flag_and_at_least_one_surface_that_reads_it() {
        for row in SURFACE_FLAGS {
            assert!(row.flag.starts_with("--"), "{}", row.flag);
            assert!(!row.used_by.is_empty(), "{}", row.flag);
        }
    }
}
