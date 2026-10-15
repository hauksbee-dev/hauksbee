//! The assertion catalogue: one table of every `kind` the spec loader accepts.
//!
//! The CLI (`hauksbee-ci explain`), the report's "why it fails" line, the server's
//! `GET /api/ci/catalog` and the app's "More checks" list all read the
//! same words. A test pins the table to the loader's closed vocabulary, so a new
//! kind cannot ship without an entry, and another runs every example through the
//! spec validator, so a documented example cannot rot. Entries are plain data:
//! add a field to [`Entry`] and every surface can show it.

use std::fmt::Write as _;

use serde_json::{json, Value};

/// One field of an assertion: its name, what it means, and the value used when absent.
pub struct Field {
    pub name: &'static str,
    pub what: &'static str,
    pub default: &'static str,
}

const fn req(name: &'static str, what: &'static str) -> Field {
    Field {
        name,
        what,
        default: "",
    }
}
const fn opt(name: &'static str, what: &'static str, default: &'static str) -> Field {
    Field {
        name,
        what,
        default,
    }
}

/// Everything known about one assertion kind.
pub struct Entry {
    pub kind: &'static str,
    pub title: &'static str,
    pub summary: &'static str,
    /// Fields the loader refuses to run without (one of a pair is noted in `what`).
    pub required: &'static [Field],
    pub optional: &'static [Field],
    /// What else the spec must declare, e.g. `firmware`, `[ac]`, `[[scenario]]`.
    pub needs: &'static [&'static str],
    /// Valid TOML that passes `hauksbee-ci check`: the `[[assert]]` plus any block it needs.
    pub example: &'static str,
    /// The usual physical or spec cause when the assertion fails.
    pub fails_because: &'static str,
}

const NAME: Field = opt(
    "name",
    "the label shown in reports",
    "built from the other fields",
);

/// Every kind, in the order the CLI and the app list them.
pub static CATALOG: &[Entry] = &[
    Entry {
        kind: "voltage",
        title: "A net stays within a voltage range",
        summary: "A net's worst dip (min) and worst peak (max) stay inside your bounds, optionally only after it settles.",
        required: &[req("net", "the net to watch"), req("min", "lowest allowed volts (or give max)")],
        optional: &[opt("max", "highest allowed volts", "no ceiling"), opt("after_ms", "only judge from this time on", "the whole run"), NAME],
        needs: &[],
        example: "[[assert]]\nkind = \"voltage\"\nnet = \"+5V\"\nmin = 4.75\nafter_ms = 50\n",
        fails_because: "the rail left its window; check the supply feeding this net and the load pulling it down.",
    },
    Entry {
        kind: "rail_window",
        title: "A rail survives a load step",
        summary: "During a [[scenario]] load step, a rail stays within bounds, dips only briefly and recovers in time.",
        required: &[req("net", "the rail to watch"), req("scenario", "the [[scenario]] id to judge over"), req("min", "brownout floor in volts (or max, or dip_below with for_max_ms)")],
        optional: &[opt("max", "ceiling in volts", "none"), opt("dip_below", "a level it may sit under only briefly", "none"), opt("for_max_ms", "how long it may sit under dip_below", "none"), opt("recover_to", "level it must return to", "none"), opt("recover_within_ms", "by this long", "none"), opt("spike_above", "overvoltage level", "none"), opt("spike_for_max_ms", "how long above spike_above", "none"), opt("settle_to", "level to settle back to after a spike", "none"), opt("settle_within_ms", "by this long", "none")],
        needs: &["[[scenario]]"],
        example: "[[scenario]]\nid = \"load_step\"\npart = \"U1\"\nprofile = \"esp32_boot_wifi\"\nstart_ms = 1.0\n\n[[assert]]\nkind = \"rail_window\"\nnet = \"+5V\"\nscenario = \"load_step\"\nmin = 3.0\n",
        fails_because: "the rail dipped/spiked or recovered/settled outside the window; check the scenario's load step and the decoupling on this net.",
    },
    Entry {
        kind: "no_faults",
        title: "Nothing is over-stressed",
        summary: "The stress monitor raised no over-current, over-voltage, over-power, reverse-bias or over-temperature fault anywhere on the board.",
        required: &[],
        optional: &[NAME],
        needs: &[],
        example: "[[assert]]\nkind = \"no_faults\"\n",
        fails_because: "the stress monitor tripped; the named component exceeded a rating (over-current / -voltage / -power / -temp / reverse-bias); check its part value and supply.",
    },
    Entry {
        kind: "max_current",
        title: "A part's current stays below a limit",
        summary: "A component's peak through-current stays under a ceiling (tracked for resistors and diodes; no_faults covers the rest).",
        required: &[req("ref", "the component, e.g. R1"), req("amps", "the ceiling in amps")],
        optional: &[NAME],
        needs: &[],
        example: "[[assert]]\nkind = \"max_current\"\nref = \"R1\"\namps = 0.02\n",
        fails_because: "the part drew more than its limit; check its load and the override value.",
    },
    Entry {
        kind: "max_temp",
        title: "A part stays below a temperature",
        summary: "A component's steady-state junction temperature (ambient + power x thermal resistance) stays under a ceiling.",
        required: &[req("ref", "the component, e.g. U1")],
        optional: &[opt("celsius", "your ceiling; omit to use the part's own datasheet Tj(max), which needs a real model", "the model's Tj(max)"), NAME],
        needs: &[],
        example: "[[assert]]\nkind = \"max_temp\"\nref = \"U1\"\ncelsius = 125\n",
        fails_because: "the junction ran hotter than the limit; check dissipation and the `ambient_c` assumption.",
    },
    Entry {
        kind: "uart",
        title: "Firmware prints a string",
        summary: "The firmware's serial output contains a string, or matches a regex.",
        required: &[req("contains", "text that must appear (or give matches)")],
        optional: &[opt("matches", "a regex instead of contains", "none"), opt("mcu", "which MCU's UART to read", "all MCUs"), NAME],
        needs: &["firmware"],
        example: "[[assert]]\nkind = \"uart\"\ncontains = \"hauksbee-demo v1\"\n",
        fails_because: "the expected UART text never appeared; check the firmware image and baud, and that the MCU booted.",
    },
    Entry {
        kind: "toggle",
        title: "A net toggles at a rate",
        summary: "A net changes at an expected frequency (a blink check), or at least a minimum number of times.",
        required: &[req("net", "the net to watch"), req("freq_hz", "expected frequency (or give min_toggles, not both)")],
        optional: &[opt("min_toggles", "a count instead of a frequency", "none"), opt("tolerance", "fraction of freq_hz, in (0, 1]", "0.25"), NAME],
        needs: &["firmware"],
        example: "[[assert]]\nkind = \"toggle\"\nnet = \"D13\"\nfreq_hz = 5.0\ntolerance = 0.4\n",
        fails_because: "the net toggled the wrong number of times; check the firmware's drive rate and the deadline window.",
    },
    Entry {
        kind: "pulse_width",
        title: "The first pulse is a set width",
        summary: "The width in milliseconds of the first complete pulse on a net; refuses if no pulse completes.",
        required: &[req("net", "the net to watch")],
        optional: &[opt("level", "high or low pulse", "high"), opt("after_ms", "ignore everything before this", "0"), opt("min", "shortest allowed width, ms", "none"), opt("max", "longest allowed width, ms", "none"), NAME],
        needs: &[],
        example: "[[assert]]\nkind = \"pulse_width\"\nnet = \"D13\"\nlevel = \"high\"\nmin = 10.15\nmax = 11.44\n",
        fails_because: "the pulse is wider or narrower than the bound, or no complete pulse fit in the run; check the timing component or lengthen duration_ms.",
    },
    Entry {
        kind: "logic_level",
        title: "A net reaches a valid logic level",
        summary: "A net reaches a valid high (or low) at every input it drives; the level is read from those parts' own cards.",
        required: &[req("net", "the net to watch")],
        optional: &[opt("level", "high or low", "high"), opt("min_width_ms", "hold the level this long", "no hold"), opt("min_toggles", "in at least this many pulses", "1"), opt("min", "a level in volts to use instead of the cards' VIH", "from the cards"), opt("max", "the same for a low", "from the cards"), NAME],
        needs: &[],
        example: "[[assert]]\nkind = \"logic_level\"\nnet = \"D13\"\nmin_width_ms = 0.001\nmin_toggles = 3\n",
        fails_because: "the net never reached a level its inputs accept, or held it too briefly; check the driver, the pull-ups and the supply the receiving parts run from.",
    },
    Entry {
        kind: "boot_coverage",
        title: "Firmware drives a control net in time",
        summary: "A gate, enable, reset or chip-select net that the firmware must actively drive to a level within a deadline of reset.",
        required: &[req("net", "the control net"), req("min", "the level in volts it must reach"), req("deadline_ms", "by this long after reset")],
        optional: &[opt("hold_ms", "how long the level must hold", "through the deadline"), NAME],
        needs: &["firmware"],
        example: "[[assert]]\nkind = \"boot_coverage\"\nnet = \"D13\"\nmin = 3.0\ndeadline_ms = 20.0\n",
        fails_because: "the firmware never drove the control net in time; check `firmware = ...` points at the right image and the net is a GPIO the firmware actually drives.",
    },
    Entry {
        kind: "protection_trip",
        title: "A supply's protection trips (or not)",
        summary: "A battery or e-fuse protection cutoff fired, or must not have fired, within a scenario window.",
        required: &[req("supply_net", "the supply's net"), req("expect_trip", "true if it should trip, false if it must not")],
        optional: &[opt("scenario", "limit to one [[scenario]] window", "the whole run"), NAME],
        needs: &["[[supply]] kind = \"battery\""],
        example: "[[supply]]\nnet = \"+5V\"\nkind = \"battery\"\nchemistry = \"liion\"\ncells = 1\ncapacity_mah = 400\nprotection_trip_a = 1.0\nprotection_delay_ms = 2.0\n\n[[assert]]\nkind = \"protection_trip\"\nsupply_net = \"+5V\"\nexpect_trip = true\n",
        fails_because: "the supply's protection did/did not latch as asserted; check the supply's limits and the load that triggers it.",
    },
    Entry {
        kind: "peripheral",
        title: "An attached device ends in a state",
        summary: "A bus device, sensor or waveform sink's end-of-run state, read by id and field (or bytes).",
        required: &[req("id", "the [[peripheral]] or [[sensor]] id"), req("field", "the field to read, with min and/or max (or give bytes)")],
        optional: &[opt("bytes", "expected memory contents, instead of field", "none"), opt("min", "lowest allowed value", "none"), opt("max", "highest allowed value", "none"), NAME],
        needs: &["[[peripheral]]"],
        example: "[[peripheral]]\nid = \"VCD\"\ntype = \"vcd_sink\"\nnets = [\"D13\"]\n\n[[assert]]\nkind = \"peripheral\"\nid = \"VCD\"\nfield = \"transitions\"\nmin = 15\nmax = 25\n",
        fails_because: "the device's end state is outside the bound; check what the firmware sent it and that the bus is wired to the right pins.",
    },
    Entry {
        kind: "phase_margin",
        title: "A loop has enough phase margin",
        summary: "The feedback loop's phase margin, in degrees, at the net you name, from a small-signal [ac] sweep.",
        required: &[req("net", "the loop break or output net"), req("min", "lowest margin in degrees, e.g. 45 (or max)")],
        optional: &[opt("max", "highest margin in degrees", "none"), NAME],
        needs: &["[ac]"],
        example: "[ac]\nfstart = 10\nfstop = 1e6\npoints = 20\n\n[[assert]]\nkind = \"phase_margin\"\nnet = \"+5V\"\nmin = 45\n",
        fails_because: "the loop's phase margin missed the bound; check the compensation network and the `[ac]` sweep range.",
    },
    Entry {
        kind: "ac_gain",
        title: "Gain at a frequency is in a band",
        summary: "A net's small-signal gain, in dB, at a frequency, from the [ac] sweep.",
        required: &[req("net", "the net to read"), req("min", "lowest gain in dB (or max)")],
        optional: &[opt("max", "highest gain in dB", "none"), opt("freq_hz", "the frequency to read at", "the sweep's reference point"), NAME],
        needs: &["[ac]"],
        example: "[ac]\nfstart = 10\nfstop = 1e6\npoints = 20\n\n[[assert]]\nkind = \"ac_gain\"\nnet = \"+5V\"\nfreq_hz = 1000\nmin = -3\n",
        fails_because: "the small-signal gain missed the band; check the AC stimulus net and the sweep points.",
    },
    Entry {
        kind: "model_coverage",
        title: "Enough of the board is modelled",
        summary: "How much of the board bound to a real device model, so a part that loses its model fails the build instead of simulating a hole.",
        required: &[req("min_critical", "fraction of active ICs that must bind, 0..1 (or min_resolved / max_active_unresolved)")],
        optional: &[opt("min_resolved", "fraction of all parts that must bind", "none"), opt("max_active_unresolved", "most unresolved parts allowed on connected nets", "none"), NAME],
        needs: &[],
        example: "[[assert]]\nkind = \"model_coverage\"\nmin_critical = 1.0\nmax_active_unresolved = 0\n",
        fails_because: "an active part has no model, so its answer is not simulated; the failure names the parts, write or extract a model for each.",
    },
    Entry {
        kind: "hwtrace",
        title: "Simulation matches a captured trace",
        summary: "The simulated waveform's features (period, rise time, overshoot) agree with a scope trace you captured on the real board.",
        required: &[req("trace", "path to the trace.toml, relative to the spec")],
        optional: &[NAME],
        needs: &["a trace file"],
        example: "[[assert]]\nkind = \"hwtrace\"\ntrace = \"trace.toml\"\n",
        fails_because: "the simulated waveform disagrees with the capture beyond tolerance; check the part values on that net and that the capture was taken at the same load.",
    },
    Entry {
        kind: "fires",
        title: "A comparator trips at a level",
        summary: "A comparator's output rises (fires) as many times as you expect, optionally first tripping at a given input level.",
        required: &[req("ref", "the comparator, e.g. U1")],
        optional: &[opt("target", "the input level in volts it must first trip at", "any"), opt("tolerance", "fraction of target, in (0, 1]", "0.05"), opt("min_toggles", "how many times it must fire", "1"), opt("reset_below", "the input must fall below this after each firing", "none"), NAME],
        needs: &["[search] (optional, for operating conditions)"],
        example: "[[assert]]\nkind = \"fires\"\nref = \"U1\"\ntarget = 0.387\ntolerance = 0.1\n",
        fails_because: "the comparator tripped at a different level, too often or not at all; the report ranks the parts that set the trip point and solves a repair value.",
    },
    Entry {
        kind: "increases",
        title: "A quantity rises as you sweep a net",
        summary: "A comparator's firing rate (or a net's average level) must rise each step along one swept net; a flat step fails too.",
        required: &[req("along", "a [[search.sweep]] net with two or more values"), req("ref", "the comparator, or give net (exactly one of the two)")],
        optional: &[opt("net", "a net whose mean level is judged, instead of ref", "none"), opt("after_ms", "ignore everything before this", "0"), NAME],
        needs: &["[search]"],
        example: "[search]\n[[search.sweep]]\nnet = \"+5V\"\nvolts = [4.5, 5.0]\n\n[[assert]]\nkind = \"increases\"\nref = \"U1\"\nalong = \"+5V\"\nafter_ms = 0.05\n",
        fails_because: "the response is flat or inverted between two neighbouring sweep points, which is a topology finding rather than a part tolerance; the report names the pair.",
    },
    Entry {
        kind: "decreases",
        title: "A quantity falls as you sweep a net",
        summary: "The mirror of increases: a comparator's firing rate (or a net's average level) must fall each step along one swept net.",
        required: &[req("along", "a [[search.sweep]] net with two or more values"), req("ref", "the comparator, or give net (exactly one of the two)")],
        optional: &[opt("net", "a net whose mean level is judged, instead of ref", "none"), opt("after_ms", "ignore everything before this", "0"), NAME],
        needs: &["[search]"],
        example: "[search]\n[[search.sweep]]\nnet = \"+5V\"\nvolts = [4.5, 5.0]\n\n[[assert]]\nkind = \"decreases\"\nref = \"U1\"\nalong = \"+5V\"\nafter_ms = 0.05\n",
        fails_because: "the response is flat or rising between two neighbouring sweep points, which is a topology finding rather than a part tolerance; the report names the pair.",
    },
];

/// The entry for `kind`, folding the `boot-coverage` alias onto `boot_coverage`.
#[must_use]
pub fn entry(kind: &str) -> Option<&'static Entry> {
    let kind = if kind == "boot-coverage" {
        "boot_coverage"
    } else {
        kind
    };
    CATALOG.iter().find(|e| e.kind == kind)
}

fn fields_json(fields: &[Field]) -> Vec<Value> {
    fields
        .iter()
        .map(|f| json!({ "name": f.name, "what": f.what, "default": f.default }))
        .collect()
}

/// The whole catalogue as the JSON the server and the app read.
#[must_use]
pub fn to_json() -> Value {
    let kinds: Vec<Value> = CATALOG
        .iter()
        .map(|e| {
            json!({
                "kind": e.kind, "title": e.title, "summary": e.summary,
                "required": fields_json(e.required), "optional": fields_json(e.optional),
                "needs": e.needs, "example": e.example, "fails_because": e.fails_because,
            })
        })
        .collect();
    json!({ "ok": true, "kinds": kinds })
}

/// The one-line-per-kind list `hauksbee-ci explain` prints.
#[must_use]
pub fn render_list() -> String {
    let mut out =
        String::from("assertion kinds (hauksbee-ci explain <kind> for fields and an example):\n");
    for e in CATALOG {
        let _ = writeln!(out, "  {:<16} {}", e.kind, e.title);
    }
    out
}

/// One entry, in full: summary, fields, what else the spec needs, an example, why it fails.
#[must_use]
pub fn render_entry(e: &Entry) -> String {
    let mut out = format!("{}: {}\n{}\n", e.kind, e.title, e.summary);
    let section = |out: &mut String, head: &str, fields: &[Field]| {
        if fields.is_empty() {
            return;
        }
        let _ = writeln!(out, "\n{head}:");
        for f in fields {
            let default = if f.default.is_empty() {
                String::new()
            } else {
                format!(" (default: {})", f.default)
            };
            let _ = writeln!(out, "  {:<18} {}{default}", f.name, f.what);
        }
    };
    section(&mut out, "required", e.required);
    section(&mut out, "optional", e.optional);
    if !e.needs.is_empty() {
        let _ = writeln!(out, "\nalso needs: {}", e.needs.join(", "));
    }
    out.push_str("\nexample:\n");
    for line in e.example.lines() {
        let _ = writeln!(out, "  {line}");
    }
    let _ = writeln!(out, "\nwhen it fails: {}", e.fails_because);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::{check_spec, CheckOptions};
    use crate::spec::{Assertion, ASSERTION_KINDS};

    /// The table and the loader's closed vocabulary list exactly the same kinds,
    /// and the loader's own `match` knows every one of them (so the list itself
    /// cannot drift from the code that enforces it).
    #[test]
    fn the_table_and_the_loader_agree_on_every_kind() {
        let mut table: Vec<&str> = CATALOG.iter().map(|e| e.kind).collect();
        let mut loader: Vec<&str> = ASSERTION_KINDS.to_vec();
        table.sort_unstable();
        loader.sort_unstable();
        assert_eq!(table, loader);
        for e in CATALOG {
            let a = Assertion {
                kind: e.kind.to_string(),
                ..Default::default()
            };
            let msg = a
                .validate()
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default();
            assert!(!msg.contains("unknown assertion kind"), "{}: {msg}", e.kind);
        }
    }

    /// Every example is a spec the validator accepts, and its required fields are
    /// the ones it actually sets.
    #[test]
    fn every_example_passes_spec_validation() {
        let dir = tempfile::tempdir().unwrap();
        for e in CATALOG {
            // The firmware key is the one thing `needs` names that lives outside the example.
            let firmware = if e.needs.contains(&"firmware") {
                "firmware = \"fw.hex\"\n"
            } else {
                ""
            };
            let spec = format!(
                "board = \"b.kicad_pcb\"\n{firmware}duration_ms = 100\n\n{}",
                e.example
            );
            let path = dir.path().join(format!("{}.toml", e.kind));
            std::fs::write(&path, &spec).unwrap();
            let diags = check_spec(
                &path,
                &CheckOptions {
                    no_board: true,
                    ..Default::default()
                },
            );
            assert!(
                diags.is_empty(),
                "{}: {:?}\n{spec}",
                e.kind,
                diags.iter().map(|d| &d.message).collect::<Vec<_>>()
            );
            for f in e.required {
                assert!(
                    e.example.contains(&format!("{} = ", f.name)),
                    "{}: example omits required `{}`",
                    e.kind,
                    f.name
                );
            }
            assert!(!e.fails_because.is_empty() && !e.title.is_empty());
        }
    }

    #[test]
    fn explain_prints_required_fields_and_an_example() {
        let text = render_entry(entry("rail_window").unwrap());
        assert!(
            text.contains("required:")
                && text.contains("scenario")
                && text.contains("kind = \"rail_window\""),
            "{text}"
        );
        assert!(entry("boot-coverage").is_some() && entry("nope").is_none());
        assert_eq!(to_json()["kinds"].as_array().unwrap().len(), CATALOG.len());
    }
}
