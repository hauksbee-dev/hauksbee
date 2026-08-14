//! Embedded, always-runnable examples.
//!
//! `hauksbee-ci run --example blinky` must work from a bare installed binary,
//! with no hauksbee checkout on disk: the error-path suggestions point here,
//! and a suggestion that only works from a source tree is a trap. The spec,
//! board and firmware are compiled in and materialized to a per-example
//! directory under the system temp dir on demand.

use std::fs;
use std::path::PathBuf;

use anyhow::Context;

/// One embedded example: its name plus the files it materializes.
struct Example {
    name: &'static str,
    /// The spec, with `board`/`firmware` paths rewritten to the materialized
    /// layout (fixed at compile time via [`SPEC_REWRITES`]).
    spec: &'static str,
    files: &'static [(&'static str, &'static [u8])],
}

#[cfg(feature = "avr")]
const BLINKY_SPEC: &str = include_str!("../examples/blinky.toml");

// Windows and explicitly permissive builds do not link GPL simavr. Keep the
// same zero-file first journey useful there without pretending firmware ran:
// the embedded name resolves to an honest static CI assertion on a passive
// divider. Full/default builds retain the four-assertion AVR co-sim example.
#[cfg(not(feature = "avr"))]
const BLINKY_SPEC: &str = include_str!("../examples/blinky-permissive.toml");

const EXAMPLES: &[Example] = &[Example {
    name: "blinky",
    spec: BLINKY_SPEC,
    files: &[
        (
            "boards/blinky.kicad_pcb",
            include_bytes!("../examples/boards/blinky.kicad_pcb"),
        ),
        (
            "demo.hex",
            include_bytes!("../../../testdata/firmware/demo/demo.hex"),
        ),
        (
            "boards/tolerance_divider.kicad_pcb",
            include_bytes!("../examples/boards/tolerance_divider.kicad_pcb"),
        ),
    ],
}];

/// The path rewrites applied to an embedded spec so it references the
/// materialized files instead of hauksbee-checkout-relative paths.
const SPEC_REWRITES: &[(&str, &str)] = &[(
    "firmware = \"../../../testdata/firmware/demo/demo.hex\"",
    "firmware = \"demo.hex\"",
)];

/// The names `--example` accepts, for help/error text.
pub fn names() -> Vec<&'static str> {
    EXAMPLES.iter().map(|e| e.name).collect()
}

/// Materialize the named example into a stable temp directory and return the
/// path of its runnable spec. Overwrites on every call, so the on-disk copy
/// can never drift stale relative to the binary.
pub fn materialize(name: &str) -> anyhow::Result<PathBuf> {
    let Some(example) = EXAMPLES.iter().find(|e| e.name == name) else {
        anyhow::bail!(
            "no embedded example named '{name}'. Available: {}",
            names().join(", ")
        );
    };
    let dir = std::env::temp_dir().join(format!("hauksbee-ci-example-{name}"));
    for (rel, bytes) in example.files {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
    }
    let mut spec = example.spec.to_string();
    for (from, to) in SPEC_REWRITES {
        if spec.contains(from) {
            spec = spec.replace(from, to);
        }
    }
    let spec_path = dir.join(format!("{name}.toml"));
    fs::write(&spec_path, &spec).with_context(|| format!("writing {}", spec_path.display()))?;
    Ok(spec_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blinky_materializes_with_resolvable_paths() {
        let spec_path = materialize("blinky").unwrap();
        let dir = spec_path.parent().unwrap();
        let spec = std::fs::read_to_string(&spec_path).unwrap();
        // Every relative path the spec references must exist next to it: that
        // is the whole point of the embedded copy.
        for line in spec.lines() {
            for key in ["board = \"", "firmware = \""] {
                if let Some(rest) = line.trim().strip_prefix(key) {
                    let rel = rest.trim_end_matches('"');
                    assert!(
                        dir.join(rel).exists(),
                        "spec references '{rel}' which was not materialized"
                    );
                }
            }
        }
        #[cfg(feature = "avr")]
        {
            // The rewrite actually fired (the embedded source uses a
            // checkout-relative firmware path).
            assert!(spec.contains("firmware = \"demo.hex\""), "{spec}");
            assert_eq!(spec.matches("[[assert]]").count(), 4, "{spec}");
        }
        #[cfg(not(feature = "avr"))]
        {
            assert!(!spec.contains("firmware ="), "{spec}");
            assert_eq!(spec.matches("[[assert]]").count(), 1, "{spec}");
        }
    }

    #[test]
    fn unknown_example_lists_the_available_names() {
        let err = materialize("nope").unwrap_err().to_string();
        assert!(err.contains("blinky"), "{err}");
    }
}
