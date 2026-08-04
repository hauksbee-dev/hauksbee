//! Trace-ampacity check wired into `--si`.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.
//!
//! The physics (IPC-2221 width -> current, the Poured-net exemption, the
//! "never invent a current" rule) all live in
//! [`hauksbee_extract::trace_current`] and are unit-tested there. What was
//! missing, and what this module supplies, is the *attribution* layer that the
//! `--si` surface needs: deciding which net carries how much current, from the
//! bound DB models, so the existing [`audit_trace_currents`] engine can be run
//! automatically instead of by hand.
//!
//! ## Attribution discipline (this is the zero-false-positive boundary)
//!
//! A current is only ever attributed to a net from an *explicit, citeable*
//! source. This module never guesses a load. The sources, each carrying its own
//! datasheet citation into the finding, are:
//!
//!   1. **A DB-modelled switching/linear converter** whose behavioural block
//!      declares an output-current limit (`iout_limit_a`): that current flows on
//!      the converter's output-pin net. Citation: the part's datasheet output
//!      current limit.
//!   2. **A part whose DB model carries a continuous-current rating**
//!      (`max_current_a`) and is a *power-delivery* part (a regulator, a
//!      connector, a load-switch FET): that rated current flows on its power
//!      net(s). Citation: the part's datasheet continuous current.
//!   3. **A part whose current the board programs** (a charger's PROG, a load
//!      switch's ILIM/ISET): the current computed from the resistor actually
//!      fitted, via the `[models.current_program]` equation, bounded by the
//!      part's own ceiling. Citation: the equation and the resistors it read.
//!      For these parts `max_current_a` is a *capability*, not a load, so it is
//!      never charged to a rail: a resistor that cannot be read produces no
//!      attribution and an info finding naming the gap. The Olimex ESP32-EVB is
//!      the worked case: it programs its MCP73833 at 200 mA through a 4.99k on
//!      PROG, and charging the 1.00 A ceiling raised High findings on two
//!      correctly-sized rails of a board that has shipped for years.
//!
//! Everything else (signal nets, parts with no current rating, poured rails) is
//! left un-attributed, and [`audit_trace_currents`] then skips it. The result is
//! a check that fires on a genuinely under-width *routed* trace carrying a cited
//! current, and stays silent on the known-good corpus, exactly as the LumenPnP
//! sweep (`trace_current_corpus.rs`) pins.

use std::collections::HashMap;

use hauksbee_extract::{
    audit_trace_currents, net_copper_from_text, ExtractedBoard, SiCheck, SiFinding, SiReport,
    SiSeverity, TraceAudit,
};
use hauksbee_models::{ComponentKind, ModelLibrary};

use crate::binder::resolve;

/// A current attributed to a net, with its citation, ready for the audit.
struct Attribution {
    current_a: f64,
    citation: String,
}

/// A part whose rail current the board programs, where the programming resistor
/// could not be read off the layout. Recorded so the hole is visible: the
/// alternative is to charge the part's ceiling to the rail, which invents a load.
struct Undetermined {
    reference: String,
    model_id: String,
    role: String,
    ceiling_a: f64,
    reason: &'static str,
}

/// What [`attribute_currents`] found: the cited currents, and the programmable
/// parts whose current it declined to guess.
struct Attributions {
    cited: HashMap<String, (f64, String)>,
    undetermined: Vec<Undetermined>,
}

/// Pin roles naming an external resistor that sets the part's operating current.
///
/// A charger's PROG, a load switch's ILIM/ISET: the part's `max_current_a` is a
/// ceiling the board chooses below. This is the same argument that already keeps
/// a FET's drain rating out of the attribution, applied to the other family of
/// parts whose rating is a capability rather than a load.
fn is_current_program_role(role: &str) -> bool {
    matches!(
        role.to_ascii_lowercase().as_str(),
        "prog" | "iprog" | "iset" | "ilim" | "ilimit" | "iref"
    )
}

/// The pin role that programs this part's current, if it has one, whether or not
/// the model states the datasheet equation.
///
/// A part with the equation gets its real current computed; a part without one is
/// still excluded from the ceiling attribution, because "we do not know the
/// constant" is not a reason to assume the maximum flows.
fn current_program_role(model: &hauksbee_models::ModelEntry) -> Option<String> {
    if let Some(cp) = &model.current_program {
        return Some(cp.pin.clone());
    }
    model
        .pins
        .values()
        .find(|r| is_current_program_role(r))
        .cloned()
}

/// How a two-terminal part on the path from a programming pin to ground behaves.
enum Series {
    /// A resistor of this many ohms.
    Ohms(f64),
    /// A closed link (a `SJ_Closed` solder jumper, a `0R`): a short.
    Closed,
    /// An open link: the path does not conduct through this part.
    Open,
    /// Anything else. The walk stops rather than assume: a diode, a fuse, a cap
    /// or an unrecognised part in this position each mean something different,
    /// and guessing which would be guessing the current.
    Unknown,
}

fn classify_series(comp: &hauksbee_extract::Component) -> Series {
    let v = comp.value.trim().to_ascii_lowercase();
    let fp = comp.footprint.to_ascii_lowercase();
    if v == "open" || fp.contains("sj_open") || fp.contains("jumper_open") {
        return Series::Open;
    }
    if matches!(v.as_str(), "closed" | "short" | "link" | "jumper" | "0r")
        || fp.contains("sj_closed")
        || fp.contains("jumper_closed")
    {
        return Series::Closed;
    }
    match hauksbee_models::value::parse_value(&comp.value) {
        // A value with a farad/henry unit in this position is not a resistor, and
        // a capacitor to ground on a PROG pin is a filter, not the programming
        // element.
        Some(p) if matches!(p.unit.as_deref(), Some("F") | Some("H")) => Series::Unknown,
        Some(p) if p.si == 0.0 => Series::Closed,
        Some(p) if p.si > 0.0 => Series::Ohms(p.si),
        _ => Series::Unknown,
    }
}

/// The resistance from `start` to ground through two-terminal parts, taking the
/// lowest-resistance path, plus the reference designators it went through.
///
/// Lowest resistance is the worst case for an ampacity question: it is the
/// largest current the programming network can select, so a trace that survives
/// it survives every jumper setting. The walk is depth-limited because the
/// topology it exists to read is short (`PROG -> R -> [link] -> GND`); a long
/// chain is not a programming network and should read as undetermined.
fn program_resistance_to_ground(
    board: &ExtractedBoard,
    start: i64,
    max_hops: usize,
) -> Option<(f64, Vec<String>)> {
    // (net, ohms so far, path), breadth-first so the shallowest paths are seen
    // first; the minimum is taken over all of them.
    let mut frontier = vec![(start, 0.0f64, Vec::<String>::new())];
    let mut best: Option<(f64, Vec<String>)> = None;
    let mut seen = vec![start];
    for _ in 0..max_hops {
        let mut next = Vec::new();
        for (net_id, ohms, path) in frontier.drain(..) {
            for comp in &board.components {
                if comp.dnp || comp.pins.len() != 2 {
                    continue;
                }
                let on_this = comp.pins.iter().any(|p| p.net == Some(net_id));
                if !on_this {
                    continue;
                }
                let Some(far) = comp
                    .pins
                    .iter()
                    .find(|p| p.net != Some(net_id))
                    .and_then(|p| p.net)
                else {
                    continue;
                };
                let add = match classify_series(comp) {
                    Series::Ohms(r) => r,
                    Series::Closed => 0.0,
                    Series::Open | Series::Unknown => continue,
                };
                let mut path = path.clone();
                path.push(comp.reference.clone());
                let total = ohms + add;
                let Some(net) = board.net(far) else { continue };
                if super::converter::is_ground_net(&net.name) {
                    if best.as_ref().is_none_or(|(b, _)| total < *b) {
                        best = Some((total, path));
                    }
                    continue;
                }
                if seen.contains(&far) {
                    continue;
                }
                seen.push(far);
                next.push((far, total, path));
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    // A dead short from the programming pin to ground is not a programmed
    // current, it is a fault or a misread network; either way the equation
    // divides by zero, so it reads as undetermined.
    best.filter(|(ohms, _)| *ohms > 0.0)
}

/// Attribute cited currents to nets from the bound DB models. Returns a map of
/// `net name -> (current, citation)` in the shape [`audit_trace_currents`] wants.
fn attribute_currents(board: &ExtractedBoard, lib: &ModelLibrary) -> Attributions {
    let mut out: HashMap<String, Attribution> = HashMap::new();
    let mut undetermined: Vec<Undetermined> = Vec::new();

    let mut consider = |net_id: Option<i64>, current_a: f64, citation: String| {
        let Some(id) = net_id else { return };
        if current_a <= 0.0 {
            return;
        }
        let Some(net) = board.net(id) else { return };
        // Take the largest attributed current per net (the worst case the net
        // must carry), keeping that source's citation.
        out.entry(net.name.clone())
            .and_modify(|a| {
                if current_a > a.current_a {
                    a.current_a = current_a;
                    a.citation = citation.clone();
                }
            })
            .or_insert(Attribution {
                current_a,
                citation,
            });
    };

    for comp in &board.components {
        if comp.dnp {
            continue;
        }
        let res = resolve(lib, comp);
        let Some(model) = res.model.as_ref() else {
            continue;
        };

        // Source 1: a converter output-current limit on the converter's out_pin.
        if let Some(conv) = &model.behavioral.converter {
            if let Some(iout) = conv.iout_limit_a {
                if let Some(net_id) = pin_net_for_role(comp, model, &conv.out_pin) {
                    consider(
                        Some(net_id),
                        iout,
                        format!(
                            "{} ({}) converter output-current limit {:.2} A [datasheet]",
                            comp.reference, model.id, iout
                        ),
                    );
                }
            }
        }

        // Source 2: a regulator / connector continuous current rating, attributed
        // to its non-ground power nets. This is deliberately narrow:
        //
        //   - A FET's `max_current_a` is a *device switch rating*, NOT a proof
        //     that its rated current flows on a board rail (a small-signal FET, a
        //     gate-drive transistor, or a load switch carrying far less all share
        //     a high Id rating). Attributing it to the FET's nets manufactures a
        //     load the design may never push, so FETs are excluded here.
        //   - A *generic / placeholder* fallback model (the power-FET coverage
        //     fallback) carries a representative rating, not a datasheet figure,
        //     so it must never seed an attribution. Excluded by id prefix.
        //
        // What remains is a regulator (whose datasheet current is the rail it
        // delivers) and a connector contact (whose rating is its through
        // current): both are real, citeable rail currents.
        if let Some(i) = model.ratings.max_current_a {
            let is_generic = model.id.starts_with("generic");
            if is_power_delivery_kind(model.kind) && !is_generic {
                // Source 2b: a part whose current the BOARD programs with one
                // resistor. Its rating is a ceiling, so the resistor decides, and
                // when the resistor cannot be read nothing is attributed. Charging
                // the ceiling here over-flags by the whole ratio between the two:
                // the Olimex ESP32-EVB programs its MCP73833 at 200 mA through a
                // 4.99k on PROG, and the 1.00 A ceiling read as a High finding on
                // two correctly-sized rails of a mass-produced board.
                if let Some(role) = current_program_role(model) {
                    let programmed = model.current_program.as_ref().and_then(|cp| {
                        let net = pin_net_for_role(comp, model, &cp.pin)?;
                        let (ohms, path) = program_resistance_to_ground(board, net, 4)?;
                        Some((cp.k_volts / ohms, ohms, path))
                    });
                    match programmed {
                        Some((current_a, ohms, path)) => {
                            // The equation cannot exceed the part's own ceiling: a
                            // small resistor does not make a charger deliver more
                            // than it is built for.
                            let current_a = current_a.min(i);
                            let citation = format!(
                                "{} ({}) charge/limit current {:.3} A programmed by {} on {} \
                                 ({:.0} ohm) [datasheet equation]",
                                comp.reference,
                                model.id,
                                current_a,
                                path.join("+"),
                                role,
                                ohms,
                            );
                            for net_id in power_nets_of(board, comp, model) {
                                consider(Some(net_id), current_a, citation.clone());
                            }
                        }
                        None => undetermined.push(Undetermined {
                            reference: comp.reference.clone(),
                            model_id: model.id.clone(),
                            role,
                            ceiling_a: i,
                            reason: if model.current_program.is_some() {
                                "no resistor path from that pin to ground could be read"
                            } else {
                                "the model states no programming equation for that pin"
                            },
                        }),
                    }
                    continue;
                }
                for net_id in power_nets_of(board, comp, model) {
                    consider(
                        Some(net_id),
                        i,
                        format!(
                            "{} ({}) continuous current rating {:.2} A [datasheet]",
                            comp.reference, model.id, i
                        ),
                    );
                }
            }
        }
    }

    Attributions {
        cited: out
            .into_iter()
            .map(|(k, a)| (k, (a.current_a, a.citation)))
            .collect(),
        undetermined,
    }
}

/// Whether a part kind delivers its rated continuous current onto a board rail
/// (so attributing `max_current_a` to its power net is physical, not a device
/// switch rating). Only a regulator (its datasheet current is the rail it
/// supplies) and a connector (its contact rating is its through current)
/// qualify; FETs are excluded because a high Id rating does not prove a high
/// load actually flows through that part on the board.
fn is_power_delivery_kind(kind: ComponentKind) -> bool {
    matches!(kind, ComponentKind::Vreg | ComponentKind::Connector)
}

/// Resolve a pin *role* (from the model's behavioural converter) to the board
/// net id on the footprint instance.
fn pin_net_for_role(
    comp: &hauksbee_extract::Component,
    model: &hauksbee_models::ModelEntry,
    role: &str,
) -> Option<i64> {
    let pad = model
        .pins
        .iter()
        .find(|(_, r)| r.eq_ignore_ascii_case(role))
        .map(|(pad, _)| pad.clone())?;
    comp.pins
        .iter()
        .find(|p| p.number == pad)
        .and_then(|p| p.net)
}

/// The *rail-carrying* power nets of a part: only the pads whose model pin role
/// is a supply/through terminal (an in/out/vbus/bat/pack rail). The part's full
/// continuous rating flows on these, and only these, an enable, feedback,
/// bypass, soft-start, sense or signal pin carries no rail current, so charging
/// it the full rating would manufacture a load and over-flag a correctly-sized
/// signal trace. Ground pins are excluded (the return is not the rail the
/// ampacity question is about).
///
/// When the model gives *no* pin roles at all (an untyped connector), we fall
/// back to "every non-ground pad", a connector's contacts are all through
/// terminals, and a rating with no pin map is the conservative reading.
fn power_nets_of(
    board: &ExtractedBoard,
    comp: &hauksbee_extract::Component,
    model: &hauksbee_models::ModelEntry,
) -> Vec<i64> {
    let has_roles = !model.pins.is_empty();
    let mut nets = Vec::new();
    for p in &comp.pins {
        let Some(id) = p.net else { continue };
        let Some(net) = board.net(id) else { continue };
        if super::converter::is_ground_net(&net.name) {
            continue;
        }
        if has_roles {
            // Charge the rating only to pads the model names as a rail terminal.
            let role = model
                .pins
                .iter()
                .find(|(pad, _)| **pad == p.number)
                .map(|(_, r)| r.as_str());
            match role {
                Some(r) if is_rail_role(r) => nets.push(id),
                // A named non-rail role (en/fb/bypass/ss/sense/data/…), or a
                // pad the model does not name, carries no rail current: skip.
                _ => {}
            }
        } else {
            nets.push(id);
        }
    }
    nets.sort_unstable();
    nets.dedup();
    nets
}

/// Whether a model pin role names a rail terminal that carries the part's
/// continuous current, a supply input, a regulated/switched output, or a
/// bus/battery through terminal. Enable, feedback, bypass, soft-start, sense,
/// reference and signal/data roles are deliberately excluded: they set or
/// monitor the rail but do not carry it.
fn is_rail_role(role: &str) -> bool {
    let r = role.to_ascii_lowercase();
    // Explicit rail terminals. `in`/`out` and their voltage-named variants,
    // plus USB VBUS, battery and pack rails.
    matches!(
        r.as_str(),
        "in" | "out"
            | "vin"
            | "vout"
            | "vcc"
            | "vdd"
            | "vbus"
            | "vsys"
            | "pvin"
            | "bat"
            | "vbat"
            | "vplus"
            | "vminus"
            | "pack_p"
            | "pack_n"
    ) || r.starts_with("vin")
        || r.starts_with("vout")
        || r.starts_with("in_out")
}

/// Run the trace-ampacity check and append its findings (and an info note) to an
/// SI report. `pcb_text` is the raw `.kicad_pcb` text (the copper geometry source);
/// when it is absent or not a KiCad layout, the check is inert.
pub fn append_ampacity(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    pcb_text: Option<&str>,
    report: &mut SiReport,
) {
    let Some(text) = pcb_text.filter(|t| t.contains("(kicad_pcb")) else {
        return;
    };
    let copper = net_copper_from_text(text);
    if copper.is_empty() {
        return;
    }
    let Attributions {
        cited,
        undetermined,
    } = attribute_currents(board, lib);
    if cited.is_empty() && undetermined.is_empty() {
        return;
    }

    let audit = TraceAudit::default();
    let findings = audit_trace_currents(&copper, &cited, &audit);

    for f in &findings {
        report.findings.push(SiFinding {
            check: SiCheck::TraceAmpacity,
            severity: SiSeverity::High,
            message: format!(
                "net '{}' narrowest routed trace {:.2} mm carries a cited {:.2} A but IPC-2221 \
                 rates that width at only {:.2} A (1 oz, {:.0} C rise); needs >= {:.2} mm. \
                 Attributed from: {}",
                f.net,
                f.min_width_mm,
                f.cited_current_a,
                f.ampacity_a,
                audit.dt_c,
                f.required_width_mm,
                f.citation,
            ),
            refs: vec![],
            nets: vec![f.net.clone()],
        });
    }

    // The coverage hole, named per part. A programmable part whose resistor could
    // not be read is a net this check did not examine, and the honest form of that
    // is to say which part and what would close it, not to fall back on the
    // ceiling and call the result a measurement.
    for u in &undetermined {
        report.findings.push(SiFinding {
            check: SiCheck::TraceAmpacity,
            severity: SiSeverity::Info,
            message: format!(
                "trace-ampacity: {} ({}) sets its current with an external resistor on {}, and \
                 {}, so its rails carry no attributed current here. Its {:.2} A rating is a \
                 ceiling, not a load, so it is not charged to the rail. To cover these rails, \
                 give the part's model a [models.current_program] equation and keep the \
                 programming resistor readable on the layout.",
                u.reference, u.model_id, u.role, u.reason, u.ceiling_a,
            ),
            refs: vec![u.reference.clone()],
            nets: vec![],
        });
    }

    // Auditable info note so the negative is on the record: how many nets carried
    // a cited current and how many were under width.
    report.findings.push(SiFinding {
        check: SiCheck::TraceAmpacity,
        severity: SiSeverity::Info,
        message: format!(
            "trace-ampacity: {} net(s) carried an attributed current; {} routed trace(s) under \
             IPC-2221 width. Poured rails are exempt (their cross-section is the plane, not the \
             discrete stubs).",
            cited.len(),
            findings.len()
        ),
        refs: vec![],
        nets: vec![],
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    use hauksbee_extract::{Component, Net, Pin};

    fn pin(number: &str, net: i64) -> Pin {
        Pin {
            number: number.into(),
            net: Some(net),
            function: String::new(),
            kind: String::new(),
            position: None,
        }
    }

    fn model_from_toml(src: &str) -> hauksbee_models::ModelEntry {
        toml::from_str(src).expect("valid model toml")
    }

    /// A one-entry library holding `toml`, written to a temp dir keyed on `tag`.
    fn lib_from(tag: &str, toml_src: &str) -> ModelLibrary {
        let dir =
            std::env::temp_dir().join(format!("hauksbee_ampacity_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("m.toml"), toml_src).unwrap();
        let mut lib = ModelLibrary::empty();
        let errs = lib.load_user_dir(&dir);
        assert!(errs.is_empty(), "test model must load: {errs:?}");
        lib
    }

    /// The MCP73833 as the Olimex ESP32-EVB fits it: 1 A ceiling, current set by
    /// a resistor on PROG.
    const CHARGER_TOML: &str = r#"
[[models]]
id = "test_charger"
kind = "vreg"
description = "test charger, current programmed on PROG"

[models.match]
value_re = "(?i)^TESTCHARGER$"

[models.params]
vout = 4.2
dropout_v = 0.3
iq_a = 0.001

[models.pins]
"1" = "in"
"2" = "gnd"
"3" = "prog"
"4" = "out"

[models.ratings]
max_current_a = 1.0

[models.current_program]
pin = "prog"
k_volts = 1000.0
"#;

    fn comp(reference: &str, value: &str, footprint: &str, pins: Vec<Pin>) -> Component {
        Component {
            reference: reference.into(),
            value: value.into(),
            lib_id: String::new(),
            footprint: footprint.into(),
            position: None,
            layer: String::new(),
            properties: vec![],
            dnp: false,
            pins,
        }
    }

    fn net(id: i64, name: &str) -> Net {
        Net {
            id,
            name: name.into(),
        }
    }

    /// Nets: 1 = +5V (in), 2 = GND, 3 = PROG, 4 = VBAT (out), 5 = the node
    /// between the programming resistor and its solder link.
    fn charger_board(prog_network: Vec<Component>) -> ExtractedBoard {
        let mut components = vec![comp(
            "U1",
            "TESTCHARGER",
            String::new().as_str(),
            vec![pin("1", 1), pin("2", 2), pin("3", 3), pin("4", 4)],
        )];
        components.extend(prog_network);
        ExtractedBoard {
            name: "charger".into(),
            nets: vec![
                net(1, "+5V"),
                net(2, "GND"),
                net(3, "PROG"),
                net(4, "VBAT"),
                net(5, "PROG_LINK"),
            ],
            components,
        }
    }

    #[test]
    fn a_programmed_charger_is_attributed_its_programmed_current_not_its_ceiling() {
        // The Olimex ESP32-EVB topology exactly: PROG -> 4.99k -> closed solder
        // jumper -> GND. I = 1000 / 4990 = 200 mA, a fifth of the 1 A ceiling,
        // and the ratio is the difference between a clean board and two High
        // findings on correctly-sized rails.
        let board = charger_board(vec![
            comp(
                "R10",
                "4.99k/1%/R0603",
                "Resistor_SMD:R_0603",
                vec![pin("1", 3), pin("2", 5)],
            ),
            comp(
                "E1",
                "Closed",
                "OLIMEX_Jumpers-FP:SJ_Closed",
                vec![pin("1", 5), pin("2", 2)],
            ),
        ]);
        let lib = lib_from("programmed", CHARGER_TOML);
        let got = attribute_currents(&board, &lib);
        assert!(
            got.undetermined.is_empty(),
            "the resistor is readable, so nothing should be undetermined: {:?}",
            got.undetermined
                .iter()
                .map(|u| u.reference.clone())
                .collect::<Vec<_>>()
        );
        for rail in ["+5V", "VBAT"] {
            let (current, citation) = got
                .cited
                .get(rail)
                .unwrap_or_else(|| panic!("{rail} should carry the programmed current"));
            // 1000 V / 4990 ohm, not a rounded 200 mA: the equation is what is
            // being tested, so the expectation is the equation.
            assert!(
                (*current - 1000.0 / 4990.0).abs() < 1e-9,
                "{rail}: expected the programmed {} A, got {current}",
                1000.0 / 4990.0
            );
            assert!(
                citation.contains("R10") && citation.contains("4990"),
                "the citation must name the resistor a reader can check: {citation}"
            );
        }
        assert!(
            !got.cited.contains_key("PROG"),
            "the programming pin itself carries no rail current"
        );
    }

    #[test]
    fn an_open_link_in_the_programming_path_attributes_nothing_and_names_the_gap() {
        // Same board with the jumper open: the charger is not programmed at all,
        // so there is no current to attribute. The ceiling must NOT step in as a
        // fallback, and the hole must be visible rather than silent.
        let board = charger_board(vec![
            comp(
                "R10",
                "4.99k",
                "Resistor_SMD:R_0603",
                vec![pin("1", 3), pin("2", 5)],
            ),
            comp(
                "E1",
                "Open",
                "OLIMEX_Jumpers-FP:SJ_Open",
                vec![pin("1", 5), pin("2", 2)],
            ),
        ]);
        let lib = lib_from("open_link", CHARGER_TOML);
        let got = attribute_currents(&board, &lib);
        assert!(
            got.cited.is_empty(),
            "an unprogrammed charger must not have its ceiling charged to a rail: {:?}",
            got.cited
        );
        assert_eq!(got.undetermined.len(), 1, "the gap must be recorded");
        let u = &got.undetermined[0];
        assert_eq!(u.reference, "U1");
        assert_eq!(u.role, "prog");
        assert!((u.ceiling_a - 1.0).abs() < 1e-9);
    }

    #[test]
    fn a_capacitor_is_not_read_as_a_programming_resistor() {
        // A filter cap from PROG to ground is not the programming element, and
        // reading it as one would produce a current from a farad value.
        let board = charger_board(vec![comp(
            "C7",
            "100nF",
            "Capacitor_SMD:C_0402",
            vec![pin("1", 3), pin("2", 2)],
        )]);
        let lib = lib_from("cap", CHARGER_TOML);
        let got = attribute_currents(&board, &lib);
        assert!(
            got.cited.is_empty(),
            "a cap programs nothing: {:?}",
            got.cited
        );
        assert_eq!(got.undetermined.len(), 1);
    }

    #[test]
    fn the_programmed_current_cannot_exceed_the_parts_ceiling() {
        // A 100 ohm resistor would put the equation at 10 A. The part is built
        // for 1 A, so the ceiling still bounds the answer.
        let board = charger_board(vec![comp(
            "R10",
            "100R",
            "Resistor_SMD:R_0603",
            vec![pin("1", 3), pin("2", 2)],
        )]);
        let lib = lib_from("clamp", CHARGER_TOML);
        let got = attribute_currents(&board, &lib);
        let (current, _) = got.cited.get("VBAT").expect("VBAT attributed");
        assert!(
            (*current - 1.0).abs() < 1e-9,
            "expected the 1 A ceiling to bound the equation, got {current}"
        );
    }

    #[test]
    fn a_regulator_with_no_programming_pin_still_carries_its_full_rating() {
        // The other side of the fix: nothing about a plain regulator changes. A
        // suppression that also silenced these would trade a false positive for a
        // missed real one.
        let toml_src = r#"
[[models]]
id = "test_ldo"
kind = "vreg"
description = "test fixed LDO, no programming pin"

[models.match]
value_re = "(?i)^TESTLDO$"

[models.params]
vout = 3.3
dropout_v = 0.2
iq_a = 0.001

[models.pins]
"1" = "in"
"2" = "gnd"
"3" = "out"

[models.ratings]
max_current_a = 1.0
"#;
        let board = ExtractedBoard {
            name: "ldo".into(),
            nets: vec![net(1, "+5V"), net(2, "GND"), net(3, "+3V3")],
            components: vec![comp(
                "U2",
                "TESTLDO",
                String::new().as_str(),
                vec![pin("1", 1), pin("2", 2), pin("3", 3)],
            )],
        };
        let lib = lib_from("plain_ldo", toml_src);
        let got = attribute_currents(&board, &lib);
        assert!(got.undetermined.is_empty());
        for rail in ["+5V", "+3V3"] {
            let (current, _) = got.cited.get(rail).unwrap_or_else(|| panic!("{rail}"));
            assert!(
                (*current - 1.0).abs() < 1e-9,
                "{rail}: the full rating must still be attributed, got {current}"
            );
        }
    }

    #[test]
    fn power_nets_of_charges_only_rail_pins_not_en_fb() {
        // A 5-pin LDO: in/gnd/en/noise_bypass/out. Its continuous rating flows
        // on the in and out rails only; the EN and noise_bypass (bypass) pins
        // carry no rail current, so power_nets_of must not return them (#11).
        let model = model_from_toml(
            r#"
                id = "lp2985_3v3"
                kind = "vreg"
                [pins]
                "1" = "in"
                "2" = "gnd"
                "3" = "en"
                "4" = "noise_bypass"
                "5" = "out"
            "#,
        );
        let comp = hauksbee_extract::Component {
            reference: "U1".into(),
            value: "LP2985-3.3".into(),
            lib_id: String::new(),
            footprint: String::new(),
            position: None,
            layer: String::new(),
            properties: vec![],
            dnp: false,
            pins: vec![
                pin("1", 10), // in  -> VIN
                pin("2", 11), // gnd -> GND
                pin("3", 12), // en  -> EN
                pin("4", 13), // noise_bypass -> BYP
                pin("5", 14), // out -> +3V3
            ],
        };
        let board = ExtractedBoard {
            name: "t".into(),
            nets: vec![
                Net {
                    id: 10,
                    name: "VIN".into(),
                },
                Net {
                    id: 11,
                    name: "GND".into(),
                },
                Net {
                    id: 12,
                    name: "EN".into(),
                },
                Net {
                    id: 13,
                    name: "BYP".into(),
                },
                Net {
                    id: 14,
                    name: "+3V3".into(),
                },
            ],
            components: vec![comp.clone()],
        };
        let nets = power_nets_of(&board, &comp, &model);
        assert_eq!(nets, vec![10, 14], "only the in/out rails carry the rating");
        assert!(!nets.contains(&11), "GND excluded");
        assert!(
            !nets.contains(&12),
            "EN must not be charged the rail current"
        );
        assert!(
            !nets.contains(&13),
            "bypass must not be charged the rail current"
        );
    }

    #[test]
    fn power_nets_of_untyped_part_falls_back_to_all_nonground() {
        // A connector with no pin roles: every non-ground contact is a through
        // terminal, so the fallback keeps the prior conservative behaviour.
        let model = model_from_toml(
            r#"id = "conn"
kind = "connector""#,
        );
        let comp = hauksbee_extract::Component {
            reference: "J1".into(),
            value: String::new(),
            lib_id: String::new(),
            footprint: String::new(),
            position: None,
            layer: String::new(),
            properties: vec![],
            dnp: false,
            pins: vec![pin("1", 20), pin("2", 21)],
        };
        let board = ExtractedBoard {
            name: "t".into(),
            nets: vec![
                Net {
                    id: 20,
                    name: "VBUS".into(),
                },
                Net {
                    id: 21,
                    name: "GND".into(),
                },
            ],
            components: vec![comp.clone()],
        };
        assert_eq!(power_nets_of(&board, &comp, &model), vec![20]);
    }

    #[test]
    fn rail_roles_recognised_signal_roles_not() {
        for r in [
            "in",
            "out",
            "vbus",
            "vin",
            "vout",
            "bat",
            "pvin",
            "vsys",
            "in_out_1a",
        ] {
            assert!(is_rail_role(r), "{r} should be a rail role");
        }
        for r in [
            "en",
            "fb",
            "noise_bypass",
            "ss",
            "sda",
            "scl",
            "ref",
            "gnd",
            "data",
        ] {
            assert!(!is_rail_role(r), "{r} must not be a rail role");
        }
    }

    #[test]
    fn power_delivery_kinds_only() {
        assert!(is_power_delivery_kind(ComponentKind::Vreg));
        assert!(is_power_delivery_kind(ComponentKind::Connector));
        // A FET's switch rating is NOT a rail-current citation: excluded.
        assert!(!is_power_delivery_kind(ComponentKind::Pmos));
        assert!(!is_power_delivery_kind(ComponentKind::Nmos));
        assert!(!is_power_delivery_kind(ComponentKind::Mcu));
        assert!(!is_power_delivery_kind(ComponentKind::Diode));
    }
}
