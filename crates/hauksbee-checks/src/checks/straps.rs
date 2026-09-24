//! Boot strapping-pin lint (static, no firmware needed).
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.
//!
//! An MCU samples certain pins at the reset latch window to choose its boot
//! mode. If a strapping pin is not at the level the part needs during that
//! window, the chip can boot wrong (the textbook case: Olimex ESP32-EVB rev D,
//! where the Ethernet PHY's free-running 50 MHz REF_CLK sits on GPIO0 - a
//! strapping pin that must be a stable HIGH - so the ESP32 randomly enters
//! download mode; ESP-IDF Ethernet API documents this exact failure, and Olimex
//! fixed it in rev E by gating PHY power until the oscillator stabilises).
//!
//! This check reads the per-part strap table from the model db
//! (`[[models.straps]]`, see `crates/hauksbee-models/db/mcu.toml`) and, for each
//! bound MCU, examines the net each strap pin sits on. It fires only on the
//! structurally unambiguous case, in line with the calibration discipline the
//! rest of hauksbee follows (zero false positives on known-good boards, or the
//! check does not ship):
//!
//!   (a) a *free-running clock source* (a powered oscillator) reaches the strap
//!       net with no strong static pull to override it -> HIGH severity, because
//!       a clock present at the reset latch is the documented failure mode and a
//!       clock is, by definition, free-running at reset;
//!   (b) the strap net is *resistively biased to the wrong level* for normal
//!       boot (a pull-up where the part needs LOW, or a pull-down where it needs
//!       HIGH) -> MEDIUM. Pulls both ways form a divider, solved from the
//!       pulls' conductances at every rail and tolerance corner against the
//!       card's sourced VIL/VIH: consistently wrong -> MEDIUM, mid-band or
//!       corner-dependent -> HIGH, no thresholds or values -> a LOW abstention;
//!   (c) (deliberately NOT fired on "no external bias": every ESP32 strap has a
//!       documented internal pull, so a bare strap pin held by the internal pull
//!       is correct - firing there would be a confident false positive on Watchy
//!       and others. See the calibration note in the docs.)
//!
//! DNP-aware: a Do-Not-Populate resistor is not assembled, so it neither biases
//! nor overrides anything. On a PCB-only extraction (no schematic pin types) the
//! clock-driver detection still works - it keys on the *component* (an
//! oscillator), not a pin electrical type - but the reduced visibility is noted.

use hauksbee_extract::{
    Component, ExtractedBoard, LintCheck, LintFinding, NetLintReport, Severity,
};
use std::collections::BTreeMap;

use hauksbee_models::value::parse_value;
use hauksbee_models::{ModelEntry, ModelLibrary, StrapInternalPull, StrapLevel, StrapPin};

use super::envelope::{role_net_id, RailRange, RailResolver};
use hauksbee_bind::binder::resolve;
use hauksbee_extract::assembly::AssemblyState;

/// Run the strap-pin lint over an extracted board, resolving each component
/// against `lib` to find MCUs that carry a strap table.
pub fn strap_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    strap_lint_with_drives(board, lib, &BTreeMap::new())
}

/// [`strap_lint`] with solved (or spec-forced) net spans, which a pull
/// divider is judged at alongside each rail's declared corners.
pub fn strap_lint_with_drives(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    solved: &BTreeMap<String, RailRange>,
) -> NetLintReport {
    let rails = RailResolver::new(board, lib, solved);
    let mut report = NetLintReport::default();
    for comp in &board.components {
        // The three-state contract: only a present part's strap table runs.
        let Some(part) = AssemblyState::of(comp).fitted() else {
            continue;
        };
        let res = resolve(lib, part);
        let Some(model) = res.model.as_ref() else {
            continue;
        };
        if model.straps.is_empty() {
            continue;
        }
        // Map each strap role to the pad that carries it, then to the net.
        for strap in &model.straps {
            // Find the pad whose role matches this strap.
            let Some(pad_num) = model
                .pins
                .iter()
                .find(|(_, role)| role.as_str() == strap.role)
                .map(|(pad, _)| pad.clone())
            else {
                continue;
            };
            let Some(pin) = comp.pins.iter().find(|p| p.number == pad_num) else {
                continue; // strap pad not present on this footprint instance
            };
            let Some(net_id) = pin.net else {
                continue; // strap pad unrouted on a PCB-only input
            };
            let Some(net) = board.net(net_id) else {
                continue;
            };
            // A floating strap net. For a pin with a documented internal pull
            // (every ESP32 strap) an undriven net settles to a defined level, so
            // skip; the prior, conservative behaviour. But a pin with NO internal
            // pull (STM32 BOOT0) left floating is genuinely undefined at reset:
            // that is the fault, not a non-event. This is the boot-mode latch the
            // firmware cannot override.
            if is_unconnected_net(&net.name) || net_is_isolated_to(board, net_id, comp) {
                if strap.internal_pull == StrapInternalPull::None {
                    // High (vs the wrong-pull arm's Medium): a wrong static pull at
                    // least boots *deterministically* wrong, so it is caught on the
                    // first power-up at the bench. A float boots correctly often
                    // enough to pass the bench yet fail intermittently in the field
                    // (humidity, EMI, a probe), a latent, unrecoverable failure
                    // that survives bring-up, which is the more dangerous outcome.
                    report.findings.push(LintFinding {
                        check: LintCheck::StrapPin,
                        severity: Severity::High,
                        message: format!(
                            "{} strap pin {} ({}) is left floating (net '{}' has no pull resistor and \
                             the pin has no internal pull): the boot level is undefined at reset, so the \
                             part may enter the bootloader instead of the application ({})",
                            comp.reference, pad_num, strap.role, net.name, strap.note
                        ),
                        refs: vec![comp.reference.clone()],
                        nets: vec![net.name.clone()],
                    });
                }
                continue;
            }
            examine_strap(board, &rails, (comp, model), strap, net_id, &mut report);
        }
    }
    report
}

/// Inspect the members of one strap net and emit a finding if it cannot hold the
/// required level at reset.
fn examine_strap(
    board: &ExtractedBoard,
    rails: &RailResolver<'_>,
    (mcu, model): (&Component, &ModelEntry),
    strap: &StrapPin,
    net_id: i64,
    report: &mut NetLintReport,
) {
    let (role, note, net_name) = (
        &strap.role,
        &strap.note,
        &board.net(net_id).map_or("", |n| &n.name),
    );
    let mut push = |severity, message: String, mut refs: Vec<String>| {
        refs.insert(0, mcu.reference.clone());
        let nets = vec![net_name.to_string()];
        let message = format!(
            "{} strap pin {role} net '{net_name}' {message} ({note})",
            mcu.reference
        );
        report.findings.push(LintFinding {
            check: LintCheck::StrapPin,
            severity,
            message,
            refs,
            nets,
        });
    };

    // (a) A free-running clock source on the strap net. This is the Olimex GPIO0
    //     fault: a powered oscillator reaches the pin (here through a small
    //     series resistor), so a clock is present at the reset latch window. We
    //     look across the net AND one series-resistor hop, because the real
    //     board puts a 10R between the oscillator and GPIO0.
    if let Some(osc) = clock_source_reaching(board, net_id).filter(|o| o.reference != mcu.reference)
    {
        let what = format!("carries a free-running clock source {} ({}): a clock present at the reset latch can mis-strap the part", osc.reference, osc.value);
        return push(Severity::High, what, vec![osc.reference.clone()]); // the clock is the dominant fault
    }

    // (b) Pull resistors biasing the strap to the WRONG level for normal boot.
    //     Only for a hard HIGH/LOW *boot-select* strap whose wrong level is
    //     unrecoverable (BOOT0, GPIO0, GPIO9, QSPI_SS). A cosmetic or
    //     flash-voltage strap (ESP32 GPIO15 boot-log, GPIO2, GPIO12) may be
    //     legitimately repurposed as an ordinary GPIO with a pull on a shipped
    //     board, so a wrong-bias finding there would be a confident false
    //     positive on a correct design - exactly the cardinal sin.
    let want_high = strap.level == StrapLevel::High;
    if !strap.boot_select || !(want_high || strap.level == StrapLevel::Low) {
        return;
    }
    let pulls = pulls_on(board, net_id);
    if pulls.iter().any(|p| p.up == want_high) && pulls.iter().any(|p| p.up != want_high) {
        // Pulls both ways: a divider sets the level, not "a correct pull exists".
        return match judge_divider(board, rails, (mcu, model), strap, &pulls) {
            Ok(None) => {}
            Ok(Some((severity, what))) => push(
                severity,
                what,
                pulls.into_iter().map(|p| p.reference).collect(),
            ),
            Err(why) => push(
                Severity::Low,
                format!(
                    "cannot judge divider ({}): {why}, so its boot level is unchecked",
                    describe(&pulls)
                ),
                Vec::new(),
            ),
        };
    }
    // One-sided: a wrong pull only counts when it is *strong* against the
    // part's internal pull (~45 kOhm on ESP32); <= 20 kOhm is a real pull.
    if let Some(p) = pulls
        .iter()
        .find(|p| p.up != want_high && p.ohms.is_some_and(|r| r <= 20_000.0))
    {
        let dir = if p.up {
            "pull-up to a rail"
        } else {
            "pull-down to ground"
        };
        let what = format!(
            "has a {dir} ({}) but normal boot needs {}: the strap is biased to the wrong level",
            p.reference,
            strap.level.as_str()
        );
        push(Severity::Medium, what, vec![p.reference.clone()]);
    }
}

/// A two-terminal resistor (ref R*, not RV/RT/RN/RP/RM, two connected pads, not
/// a ferrite/inductor), and assembled (not DNP). Mirrors the extract lint's
/// `is_resistor` plus a DNP guard. Shared with the boot check's bias-resistor
/// predicate, a varistor (RV) or thermistor (RT) to ground sets no DC level
/// and must never be credited as a bias.
pub(crate) fn is_assembled_resistor(c: &Component) -> bool {
    if !AssemblyState::of(c).is_present() {
        return false;
    }
    let r = c.reference.to_ascii_uppercase();
    let lib = c.lib_id.to_ascii_lowercase();
    let is_r_ref = r.starts_with('R')
        && !r.starts_with("RV")
        && !r.starts_with("RT")
        && !r.starts_with("RN")
        && !r.starts_with("RP")
        && !r.starts_with("RM");
    let connected = c.pins.iter().filter(|p| p.net.is_some()).count();
    is_r_ref && connected == 2 && !lib.contains("ferrite") && !lib.contains("inductor")
}

/// Is this component a *powered oscillator* (a free-running clock source), as
/// opposed to a bare crystal (a passive resonator that does not drive on its
/// own)? An oscillator is a packaged part with its own VDD; we key on the
/// reference / value / lib_id, and require it to NOT be a plain 2-terminal
/// crystal. DNP oscillators are not assembled, so they do not drive.
fn is_clock_oscillator(c: &Component) -> bool {
    if !AssemblyState::of(c).is_present() {
        return false;
    }
    let r = c.reference.to_ascii_uppercase();
    let v = c.value.to_ascii_uppercase();
    let lib = c.lib_id.to_ascii_lowercase();
    // An oscillator has >= 3 connected pads (OUT/GND/VDD, often +OE); a bare
    // crystal is 2-terminal. Refuse 2-terminal parts outright.
    let connected = c.pins.iter().filter(|p| p.net.is_some()).count();
    if connected < 3 {
        return false;
    }
    // The >= 3-pad gate (a powered oscillator, not a 2-terminal crystal) plus
    // the rail/ground hop-exclusion in clock_source_reaching is what keeps this
    // sound. The value-string match below is the soft edge: a 3+-pad part whose
    // value merely contains "MHZ"/"OSC" could be mis-classed, but it only
    // matters if it ALSO sits on a strap signal net (or one series-R away), and
    // a free-running clock that close to a strap pin is the fault we want. The
    // lib_id "oscillator" match is the strong signal; the rest are value/ref
    // heuristics for parts whose symbol library does not say "oscillator".
    lib.contains("oscillator")
        || v.contains("OSCILLATOR")
        || v.contains("MHZ")
        || v.contains("OSC")
        || v.contains("XO")
        // Olimex's value string is "Q50MHz/..."; ref CR* / X* / Y* / OSC* are
        // the usual oscillator designators.
        || r.starts_with("OSC")
        || r.starts_with("CR")
        || (r.starts_with('X') && v.contains("MHZ"))
        || (r.starts_with('Y') && v.contains("MHZ"))
}

/// Find a free-running clock source whose *output* reaches `net_id` either
/// directly or through exactly one assembled series resistor (the real board
/// puts a 10R between the oscillator output and GPIO0).
///
/// The hop deliberately never travels through a power rail or ground: a clock
/// signal does not propagate via VDD/GND, and an oscillator's VDD pin sits on a
/// rail. Without that guard, a strap with an ordinary pull-up to +3V3 would
/// "reach" the oscillator's VDD on the same rail and false-fire (the exact bug
/// the GPIO15 pull-up exposed during calibration). So the oscillator must be
/// reached on a *signal* net, via a pad that is not on a rail/ground.
fn clock_source_reaching(board: &ExtractedBoard, net_id: i64) -> Option<&Component> {
    // The strap net itself must not be a rail/ground (it never is for a strap,
    // but guard anyway so the membership scan is meaningful).
    if let Some(n) = board.net(net_id) {
        if is_ground_name(&n.name) || is_rail_name(&n.name) {
            return None;
        }
    }
    // Direct: an oscillator drives this signal net via a non-power pad.
    if let Some(c) = oscillator_driving_net(board, net_id) {
        return Some(c);
    }
    // One series-resistor hop: a resistor with one pad on the strap net and the
    // other pad on a *signal* net (not a rail/ground) that an oscillator drives.
    for (c, _p) in board.net_members(net_id) {
        if !is_assembled_resistor(c) {
            continue;
        }
        for op in &c.pins {
            let Some(oid) = op.net else { continue };
            if oid == net_id {
                continue;
            }
            // Never hop through a power rail or ground.
            if let Some(on) = board.net(oid) {
                if is_ground_name(&on.name) || is_rail_name(&on.name) {
                    continue;
                }
            }
            if let Some(c) = oscillator_driving_net(board, oid) {
                return Some(c);
            }
        }
    }
    None
}

/// An assembled oscillator that touches `net_id` via a pad that is NOT one of
/// its power pads (a clock OUTPUT, not its VDD/GND). We identify power pads by
/// the net they sit on (rail/ground), since PCB-only inputs carry no pin
/// functions. So: the oscillator drives this net iff one of its pads is on
/// `net_id` and that net is not a rail/ground.
fn oscillator_driving_net(board: &ExtractedBoard, net_id: i64) -> Option<&Component> {
    let net = board.net(net_id)?;
    if is_ground_name(&net.name) || is_rail_name(&net.name) {
        return None;
    }
    for (c, _p) in board.net_members(net_id) {
        if is_clock_oscillator(c) {
            return Some(c);
        }
    }
    None
}

/// One assembled resistor from the strap net to a rail (`up`) or to ground.
struct Pull {
    reference: String,
    value: String,
    far: i64,
    up: bool,
    /// `None` when the value string does not parse to ohms.
    ohms: Option<f64>,
    /// Fractional tolerance when the part states one ("10k 1%", or a
    /// `Tolerance` property); zero when unknown.
    tol: f64,
}

fn pulls_on(board: &ExtractedBoard, net_id: i64) -> Vec<Pull> {
    let mut pulls = Vec::new();
    for (c, _) in board
        .net_members(net_id)
        .into_iter()
        .filter(|(c, _)| is_assembled_resistor(c))
    {
        for far in c
            .pins
            .iter()
            .filter_map(|p| p.net)
            .filter(|id| *id != net_id)
        {
            let Some(name) = board.net(far).map(|n| n.name.as_str()) else {
                continue;
            };
            if !(is_ground_name(name) || is_rail_name(name)) {
                continue;
            }
            let ohms =
                parse_value(&c.value).filter(|v| matches!(v.unit.as_deref(), None | Some("Ω")));
            let tol = c
                .properties
                .iter()
                .filter(|(k, _)| k.eq_ignore_ascii_case("tolerance"))
                .map(|(_, v)| v.as_str());
            let tol = tol
                .chain(c.value.split_whitespace())
                .find_map(|t| t.trim().strip_suffix('%')?.parse::<f64>().ok());
            let (reference, value, up) =
                (c.reference.clone(), c.value.clone(), !is_ground_name(name));
            pulls.push(Pull {
                reference,
                value,
                far,
                up,
                ohms: ohms.map(|v| v.si),
                tol: tol.unwrap_or(0.0) / 100.0,
            });
        }
    }
    pulls
}

fn describe(pulls: &[Pull]) -> String {
    let side = |p: &Pull| if p.up { "up" } else { "down" };
    pulls
        .iter()
        .map(|p| format!("{} {} {}", p.reference, p.value, side(p)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Judge a pull divider on a boot-select strap: the pin voltage from the
/// pulls' conductances, at every combination of each rail's solved level and
/// declared corners and at both resistor-tolerance extremes, against the
/// card's sourced VIL/VIH at that corner's VDD. `Ok(None)` when every corner
/// reads the required level; `Err` names what is missing to judge it at all.
fn judge_divider(
    board: &ExtractedBoard,
    rails: &RailResolver<'_>,
    (mcu, model): (&Component, &ModelEntry),
    strap: &StrapPin,
    pulls: &[Pull],
) -> Result<Option<(Severity, String)>, String> {
    if let Some(p) = pulls.iter().find(|p| p.ohms.is_none()) {
        return Err(format!(
            "{} value '{}' is not a resistance",
            p.reference, p.value
        ));
    }
    let (Some([il_k, il_v]), Some([ih_k, ih_v])) = (strap.vil, strap.vih) else {
        return Err(format!(
            "the '{}' card carries no sourced VIL/VIH for {}",
            model.id, strap.role
        ));
    };
    let vdd = role_net_id(mcu, model, "vdd")
        .ok_or_else(|| format!("the '{}' card names no vdd pin to scale VIL/VIH", model.id))?;
    // The rails in play (the vdd net shares a corner with a pull to it).
    let mut nets: Vec<(i64, Vec<f64>)> = Vec::new();
    for id in pulls.iter().filter(|p| p.up).map(|p| p.far).chain([vdd]) {
        let name = board.net(id).map_or("?", |n| n.name.as_str());
        let corners = rails.corners(id);
        if corners.is_empty() {
            return Err(format!("rail '{name}' has no known voltage"));
        }
        if !nets.iter().any(|(n, _)| *n == id) {
            nets.push((id, corners));
        }
    }
    let combos: usize = nets.iter().map(|(_, c)| c.len()).product();
    let widen = |(a, b): (f64, f64), v: f64| (a.min(v), b.max(v));
    let empty = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut pin, mut il, mut ih) = (empty, empty, empty);
    let mut reads = [false; 3]; // low, undefined, high
    for mut k in 0..combos {
        let mut level = BTreeMap::new();
        for (id, corners) in &nets {
            level.insert(*id, corners[k % corners.len()]);
            k /= corners.len();
        }
        let (vil, vih) = (il_k * level[&vdd] + il_v, ih_k * level[&vdd] + ih_v);
        (il, ih) = (widen(il, vil), widen(ih, vih));
        for skew in [-1.0, 1.0] {
            // A 0 Ohm part is a short: a micro-ohm pins the net to its far side.
            let (mut g, mut i) = (0.0, 0.0);
            for p in pulls {
                let r = (p.ohms.unwrap_or_default()
                    * (1.0 + skew * p.tol * if p.up { -1.0 } else { 1.0 }))
                .max(1e-6);
                g += 1.0 / r;
                i += level.get(&p.far).copied().unwrap_or(0.0) / r;
            }
            let v = i / g;
            pin = widen(pin, v);
            reads[if v <= vil {
                0
            } else if v >= vih {
                2
            } else {
                1
            }] = true;
        }
    }
    let want_high = strap.level == StrapLevel::High;
    let (verdict, severity) = match reads {
        [true, false, false] if !want_high => return Ok(None),
        [false, false, true] if want_high => return Ok(None),
        [true, false, false] => ("LOW at every corner", Severity::Medium),
        [false, false, true] => ("HIGH at every corner", Severity::Medium),
        [false, true, false] => ("an undefined level (between VIL and VIH)", Severity::High),
        _ => ("an undefined, corner-dependent level", Severity::High),
    };
    let span = |(a, b): (f64, f64)| {
        if (b - a).abs() < 5e-3 {
            format!("{a:.2} V")
        } else {
            format!("{a:.2}-{b:.2} V")
        }
    };
    Ok(Some((severity, format!(
        "is a pull divider ({}) at {} across {combos} rail corner(s) and the resistor tolerances, against VIL <= {} and VIH >= {}: it reads {verdict}, but normal boot needs {}",
        describe(pulls), span(pin), span(il), span(ih), strap.level.as_str()
    ))))
}

// ── Small net-name helpers (local copies; the extract versions are private) ──

fn norm(name: &str) -> String {
    let n = name.trim();
    let leaf = n.rsplit('/').next().unwrap_or(n);
    leaf.trim().to_ascii_uppercase()
}

fn is_ground_name(name: &str) -> bool {
    let n = norm(name);
    matches!(
        n.as_str(),
        "GND" | "GNDA" | "GNDD" | "AGND" | "DGND" | "PGND" | "VSS" | "GNDIO" | "0"
    ) || n.starts_with("GND")
}

/// Is this net name a power rail? Every use in this file is boolean ("is the
/// far side of this pull / this hop a rail"), so this is a predicate, not a
/// voltage table. Breadth matches boot.rs's `is_power_or_ground_net`, and for
/// the same calibration reason: an exact-name table (3V3/5V/1V8 plus
/// VCC/VBUS-gated forms) misses VBAT/VSYS/VMOT/VIN and bare voltages (9V/12V),
/// so a correct pull-up to "VBAT" reads as a signal net, which both lets the
/// clock-reach hop wander onto a rail (a spurious HIGH clock finding through an
/// oscillator's VDD) and hides a genuine wrong-direction pull from `wrong_pull`.
fn is_rail_name(name: &str) -> bool {
    let n = norm(name);
    // Ground is its own family, never a rail.
    if is_ground_name(name) {
        return false;
    }
    // Explicit '+' rail (e.g. "+3V3", "+5V", "+12V").
    if n.starts_with('+') {
        return true;
    }
    // V-prefixed rails (VCC/VDD/VBAT/VMOT/VSYS/VIN/VIO/VREF…) and bare voltage
    // names starting with a digit and carrying a 'V' ("12V", "3V3", "9V").
    let v_named = n.starts_with('V') && n.len() >= 2;
    let voltage_named = n.contains('V') && n.chars().next().is_some_and(|c| c.is_ascii_digit());
    v_named || voltage_named
}

fn is_unconnected_net(name: &str) -> bool {
    name.trim_start_matches('/').starts_with("unconnected-")
}

/// True when the only thing on `net_id` is `mcu` itself, no resistor, no other
/// component, nothing that could define a level. Such a strap net is floating
/// just as surely as a KiCad `unconnected-*` net, even if it carries a real name.
/// A net with ANY other member (a pull resistor, a header, a jumper, another
/// driver) is NOT isolated, so a jumper-selectable BOOT0 (a resistor to a 3-pin
/// header, as on dev boards) never trips this, only a genuinely dangling pin.
fn net_is_isolated_to(board: &ExtractedBoard, net_id: i64, mcu: &Component) -> bool {
    board
        .net_members(net_id)
        .iter()
        .all(|(c, _)| c.reference == mcu.reference)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{board, part};
    use super::*;
    use Severity::{High, Low, Medium};

    /// A test MCU whose active-low boot strap carries absolute thresholds
    /// (VIL 0.9 V, VIH 2.0 V), so divider corners are easy to place by hand.
    const STRAP_CARD: &str = r#"
[[models]]
id = "strap_test_mcu"
kind = "mcu"
description = "divider fixture"
[models.match]
value_re = "^STRAPTEST$"
[models.pins]
"1" = "vdd"
"2" = "boot0"
[[models.straps]]
role = "boot0"
level = "low"
boot_select = true
internal_pull = "none"
vil = [0.0, 0.9]
vih = [0.0, 2.0]
"#;

    fn library() -> ModelLibrary {
        let dir = std::env::temp_dir().join(format!("hauksbee_straps_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("models.toml"), STRAP_CARD).unwrap();
        let mut lib = ModelLibrary::builtin();
        assert!(lib.load_user_dir(&dir).is_empty(), "test card must load");
        lib
    }

    /// Nets: 1 GND, 2 the rail, 3 the strap, 4/7 oscillator outputs, 6 enable.
    fn strap_board(rail: &str, strap: &str, parts: Vec<Component>) -> ExtractedBoard {
        let nets = [
            (1, "GND"),
            (2, rail),
            (3, strap),
            (4, "Net-(CR1-OUT)"),
            (6, "/OSC_EN"),
            (7, "Net-(CR2-OUT)"),
        ];
        board(&nets, parts)
    }
    /// A pull from the strap (net 3) to `far` (1 = GND, 2 = the rail).
    fn pull(reference: &str, value: &str, far: i64) -> Component {
        part(reference, value, &[("1", 3), ("2", far)])
    }
    fn osc(reference: &str, out: i64) -> Component {
        part(
            reference,
            "Q50MHz/25ppm/3V/4P",
            &[("1", 6), ("2", 1), ("3", out), ("4", 2)],
        )
    }
    fn esp32(strap_pad: &str) -> Component {
        part("U3", "ESP-WROOM-32", &[("2", 2), (strap_pad, 3)])
    }
    fn stm32() -> Component {
        part("U1", "STM32F103C8T6", &[("9", 2), ("8", 1), ("44", 3)])
    }
    fn test_mcu() -> Component {
        part("U9", "STRAPTEST", &[("1", 2), ("2", 3)])
    }

    fn check(
        name: &str,
        b: &ExtractedBoard,
        drives: &BTreeMap<String, RailRange>,
        want: &[(Severity, &[&str])],
    ) {
        let report = strap_lint_with_drives(b, &library(), drives);
        let got: Vec<_> = report.of_check(LintCheck::StrapPin).collect();
        assert_eq!(got.len(), want.len(), "{name}: {got:#?}");
        for (finding, (severity, needles)) in got.iter().zip(want) {
            assert_eq!(finding.severity, *severity, "{name}: {}", finding.message);
            for needle in *needles {
                assert!(
                    finding.message.contains(needle),
                    "{name}: '{needle}' not in {}",
                    finding.message
                );
            }
        }
    }

    #[test]
    fn strap_cases() {
        let none = BTreeMap::new();
        let with_unrelated_osc = |rail| {
            strap_board(
                rail,
                "/GPIO0",
                vec![esp32("25"), pull("R36", "10k", 2), osc("CR2", 7)],
            )
        };
        let jumper = board(
            &[(1, "GND"), (2, "+3V3"), (3, "/BOOT0"), (4, "/BOOT0_SEL")],
            vec![
                part("U1", "STM32F401CCU6", &[("44", 3)]),
                pull("R1", "1k", 4),
                part("J1", "BOOT_SEL", &[("1", 2), ("2", 4), ("3", 1)]),
            ],
        );
        let cases: Vec<(&str, ExtractedBoard, &[(Severity, &[&str])])> = vec![
            // The Olimex fault: a 50 MHz oscillator through a 10R onto GPIO0.
            (
                "clock on GPIO0",
                strap_board(
                    "+3V3",
                    "/GPIO0",
                    vec![
                        esp32("25"),
                        osc("CR1", 4),
                        part("R36", "10R", &[("1", 4), ("2", 3)]),
                    ],
                ),
                &[(High, &["gpio0", "CR1", "GPIO0"])],
            ),
            (
                "pulled GPIO0",
                strap_board("+3V3", "/GPIO0", vec![esp32("25"), pull("R36", "10k", 2)]),
                &[],
            ),
            // Sharing only the rail with an oscillator's VDD is not a clock path,
            // whether the rail is named +3V3 or VBAT (bug #19).
            ("osc shares +3V3", with_unrelated_osc("+3V3"), &[]),
            ("osc shares VBAT", with_unrelated_osc("VBAT"), &[]),
            (
                "BOOT0 pulled up",
                strap_board("+3V3", "/BOOT0", vec![stm32(), pull("R1", "10k", 2)]),
                &[(Medium, &["boot0", "wrong level"])],
            ),
            (
                "BOOT0 pulled to VBAT (bug #20)",
                strap_board("VBAT", "/BOOT0", vec![stm32(), pull("R1", "10k", 2)]),
                &[(Medium, &["boot0", "wrong level"])],
            ),
            (
                "BOOT0 pulled down",
                strap_board("+3V3", "/BOOT0", vec![stm32(), pull("R1", "10k", 1)]),
                &[],
            ),
            // STM32 BOOT0 has no internal pull: floating is undefined at reset,
            // whether KiCad names the net unconnected-* or it is merely isolated.
            (
                "floating BOOT0",
                strap_board(
                    "+3V3",
                    "unconnected-(U1-Pad44)",
                    vec![part("U1", "STM32F401CCU6", &[("44", 3)])],
                ),
                &[(High, &["floating", "Pad44"])],
            ),
            (
                "isolated BOOT0",
                strap_board(
                    "+3V3",
                    "/BOOT0",
                    vec![part("U1", "STM32F401CCU6", &[("44", 3)])],
                ),
                &[(High, &["floating"])],
            ),
            ("jumper-selected BOOT0", jumper, &[]),
            // ESP32 straps have an internal pull, so an undriven GPIO0 is defined.
            (
                "floating GPIO0",
                strap_board("+3V3", "unconnected-(U3-Pad25)", vec![esp32("25")]),
                &[],
            ),
            // GPIO15 is not boot_select: a board may repurpose it with a pull.
            (
                "GPIO15 pulled down",
                strap_board(
                    "+3V3",
                    "/GPIO15_LED",
                    vec![esp32("23"), pull("R1", "10k", 1)],
                ),
                &[],
            ),
            (
                "AVR has no straps",
                strap_board(
                    "+5V",
                    "PC6_RESET",
                    vec![part("U1", "ATmega328P", &[("1", 3), ("7", 2), ("8", 1)])],
                ),
                &[],
            ),
            // Dividers. A strong correct pull against a weak opposing one: GPIO0
            // sits at 2.85-3.15 V over the +3V3 corners, above 0.75 x VDD.
            (
                "GPIO0 1k up / 10k down",
                strap_board(
                    "+3V3",
                    "/GPIO0",
                    vec![esp32("25"), pull("R1", "1k", 2), pull("R2", "10k", 1)],
                ),
                &[],
            ),
            (
                "active-low 1k down / 10k up",
                strap_board(
                    "+3V3",
                    "/BOOT0",
                    vec![test_mcu(), pull("R1", "10k", 2), pull("R2", "1k", 1)],
                ),
                &[],
            ),
            // The audit case: 1k up and 10k down on an active-low strap reads HIGH.
            (
                "active-low 1k up / 10k down",
                strap_board(
                    "+3V3",
                    "/BOOT0",
                    vec![test_mcu(), pull("R1", "1k", 2), pull("R2", "10k", 1)],
                ),
                &[(
                    Medium,
                    &["2.85-3.15 V", "reads HIGH at every corner", "needs low"],
                )],
            ),
            (
                "mid-band 10k / 10k",
                strap_board(
                    "+3V3",
                    "/BOOT0",
                    vec![test_mcu(), pull("R1", "10k", 2), pull("R2", "10k", 1)],
                ),
                &[(
                    High,
                    &["1.57-1.73 V", "VIL <= 0.90 V", "VIH >= 2.00 V", "undefined"],
                )],
            ),
            // 27k/10k crosses VIL inside the +3V3 envelope: 0.85 V at 3.135 V but
            // 0.94 V at 3.465 V, so the level depends on the corner.
            (
                "corner-dependent 27k / 10k",
                strap_board(
                    "+3V3",
                    "/BOOT0",
                    vec![test_mcu(), pull("R1", "27k 1%", 2), pull("R2", "10k 1%", 1)],
                ),
                &[(High, &["corner-dependent"])],
            ),
            (
                "unreadable divider value",
                strap_board(
                    "+3V3",
                    "/BOOT0",
                    vec![test_mcu(), pull("R1", "10k", 2), pull("R2", "XYZ", 1)],
                ),
                &[(Low, &["cannot judge divider", "R2 value 'XYZ'"])],
            ),
            // STM32F103's card has no sourced BOOT0 thresholds: abstain, visibly.
            (
                "no thresholds",
                strap_board(
                    "+3V3",
                    "/BOOT0",
                    vec![stm32(), pull("R1", "1k", 2), pull("R2", "10k", 1)],
                ),
                &[(Low, &["cannot judge divider", "no sourced VIL/VIH"])],
            ),
        ];
        for (name, b, want) in &cases {
            check(name, b, &none, want);
        }
        // A solved span joins the declared corners: +3V3 sagging to 2.6 V in the
        // run pulls the 1k/10k active-low divider down to 2.36 V, still HIGH.
        let solved = BTreeMap::from([(
            "+3V3".to_string(),
            RailRange {
                min_v: 2.6,
                max_v: 3.3,
            },
        )]);
        let faulty = strap_board(
            "+3V3",
            "/BOOT0",
            vec![test_mcu(), pull("R1", "1k", 2), pull("R2", "10k", 1)],
        );
        check(
            "solved rail",
            &faulty,
            &solved,
            &[(Medium, &["2.36-3.15 V", "4 rail corner(s)"])],
        );
    }
}
