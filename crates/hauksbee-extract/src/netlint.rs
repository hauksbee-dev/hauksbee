//! Connectivity lint: design-rule checks that read the netlist graph (nets,
//! components, pin functions) rather than copper geometry. These are the
//! "lint-class" checks that catch wiring intent errors a value/short sweep
//! misses: missing I2C pull-ups, floating active-control pins (enable / reset /
//! chip-select left on a single-pin net), and LED series-resistor current
//! sanity.
//!
//! Design priority is **low false-positive rate**. Every check is conservative:
//! it fires only when the structural evidence is unambiguous, and it carries the
//! exact nets / refs / pins so a finding can be chased to the source. Decoupling
//! absence is deliberately NOT a check (too noisy to be useful).
//!
//! Works on any extraction path. Pin-function-aware checks (control-pin
//! floating) need `Pin.function` populated, which the KiCad netlist, newer
//! `.kicad_pcb`, and schematic paths provide; net-name-based checks (I2C
//! pull-ups, LED current) work even on pin-function-less inputs.

use crate::{Component, ExtractedBoard, Pin};

/// One lint finding, severity-tagged, with the evidence to reproduce it.
#[derive(Debug, Clone)]
pub struct LintFinding {
    pub check: LintCheck,
    pub severity: Severity,
    /// Human-readable one-line description.
    pub message: String,
    /// Reference designators implicated (e.g. the IC with the floating EN, or
    /// the resistor + LED of an over-driven indicator).
    pub refs: Vec<String>,
    /// Net(s) implicated by name.
    pub nets: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LintCheck {
    /// An I2C bus net (SDA/SCL) with no resistor to a power rail.
    MissingI2cPullup,
    /// An active-control pin (enable / reset / chip-select / output-enable) on a
    /// net with no other connection - genuinely floating, no defined level.
    FloatingControlPin,
    /// An LED + series resistor whose computed forward current is outside the
    /// sane indicator band.
    LedCurrentSanity,
    /// Two push-pull output (or power-output) pins of different parts tied
    /// directly on one net with nothing to resolve the contention - both can
    /// drive the net to opposite levels and fight. A schematic-stage ERC check
    /// built from extracted pin electrical types.
    OutputContention,
    /// An MCU boot strapping pin (per the model db's per-part strap table) whose
    /// net cannot hold the level the part needs at the reset latch window: a
    /// free-running clock source on it, a pull to the wrong rail, or no defined
    /// level where one is required. Produced by the engine-layer strap lint.
    StrapPin,
    /// Two board-level functions are routed to different MCU pins, but those
    /// pins map to the same shared silicon resource instance inside the MCU (a
    /// PWM slice/channel, a QSPI pin group), so the MCU cannot serve both at
    /// once. Produced by `resource_conflict.rs`, reported through this same
    /// `NetLintReport` shape.
    McuResourceConflict,
    /// A passive reference designator disagrees with the passive footprint
    /// family, e.g. R1 on `Capacitor_SMD:C_0603`.
    DesignatorFootprintMismatch,
    /// A passive value is physically implausible for the stated package.
    ValuePackageSanity,
    /// A passive still has a placeholder / unset value (`R`, `C`, `L`, `?`,
    /// empty), so downstream physics cannot know the actual part.
    PlaceholderValue,
    /// A component that looks like an MCU (by part-number family) resolved to no
    /// device model, so the MCU-model-dependent checks — the boot strap-pin lint
    /// and the internal resource-conflict check — could not run on it. Without
    /// this note those checks are silently empty, which a bare "Looks healthy"
    /// verdict would misread as "checked and clean". Informational, never a
    /// `--strict` failure: an unmodelled part is a coverage gap, not a defect.
    UncheckedMcu,
    /// A configurable controller whose strap / divider resistors select a
    /// documented operating mode decodes (against the part's datasheet bands) to
    /// the WRONG mode, or to an internally-inconsistent configuration. Per-part,
    /// grows incrementally; seeded with the CYPD3177 USB-C PD sink. Produced by
    /// the engine-layer `device_decode` check.
    DeviceDecode,
}

impl LintCheck {
    pub fn as_str(self) -> &'static str {
        match self {
            LintCheck::MissingI2cPullup => "missing_i2c_pullup",
            LintCheck::FloatingControlPin => "floating_control_pin",
            LintCheck::LedCurrentSanity => "led_current_sanity",
            LintCheck::OutputContention => "output_contention",
            LintCheck::StrapPin => "strap_pin",
            LintCheck::McuResourceConflict => "mcu_resource_conflict",
            LintCheck::DesignatorFootprintMismatch => "designator_footprint_mismatch",
            LintCheck::ValuePackageSanity => "value_package_sanity",
            LintCheck::PlaceholderValue => "placeholder_value",
            LintCheck::UncheckedMcu => "unchecked_mcu",
            LintCheck::DeviceDecode => "device_decode",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Would ship a functional failure.
    High,
    /// Degrades margin / robustness; works in the nominal case.
    Medium,
    /// Ugly practice, unlikely to bite.
    Low,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::High => "high",
            Severity::Medium => "medium",
            Severity::Low => "low",
        }
    }
}

/// The full lint report.
#[derive(Debug, Clone, Default)]
pub struct NetLintReport {
    pub findings: Vec<LintFinding>,
}

impl NetLintReport {
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }
    pub fn count(&self) -> usize {
        self.findings.len()
    }
    pub fn of_check(&self, c: LintCheck) -> impl Iterator<Item = &LintFinding> {
        self.findings.iter().filter(move |f| f.check == c)
    }
}

impl ExtractedBoard {
    /// Run the connectivity lint-class checks.
    pub fn net_lint(&self) -> NetLintReport {
        let mut report = NetLintReport::default();
        check_i2c_pullups(self, &mut report);
        check_floating_control_pins(self, &mut report);
        check_led_current(self, &mut report);
        check_output_contention(self, &mut report);
        check_design_file_qc(self, &mut report);
        report
    }
}

// ---------------------------------------------------------------------------
// Net classification helpers (kept local so the lint is self-contained).
// ---------------------------------------------------------------------------

/// Normalise a net name: trim, drop a leading hierarchical path and `/`, upper.
fn norm(name: &str) -> String {
    let n = name.trim();
    // Keep only the leaf of a hierarchical net path so "/Power/+3V3" -> "+3V3".
    let leaf = n.rsplit('/').next().unwrap_or(n);
    leaf.trim().to_ascii_uppercase()
}

/// Ground net?
fn is_ground(name: &str) -> bool {
    let n = norm(name);
    matches!(
        n.as_str(),
        "GND" | "GNDA" | "GNDD" | "AGND" | "DGND" | "PGND" | "VSS" | "GNDIO" | "0"
    ) || n.starts_with("GND")
}

/// Power-rail nominal voltage by net name, mirroring the engine binder's table.
/// Returns `None` for non-rail nets.
fn rail_voltage(name: &str) -> Option<f64> {
    let n = norm(name);
    match n.as_str() {
        "+5V" | "5V" | "VCC" | "VDD" | "+VCC" | "VBUS" | "+5V0" | "VCC5V" | "VCC5" => Some(5.0),
        "+3V3" | "3V3" | "+3.3V" | "3.3V" | "VCC3V3" | "VDD3V3" | "VDD3P3" | "+3V3A" => Some(3.3),
        "+3V" | "3V" | "+3V0" | "3V0" => Some(3.0),
        "+1V8" | "1V8" | "1.8V" | "VDD1V8" => Some(1.8),
        "+2V8" | "2V8" => Some(2.8),
        "+12V" | "12V" => Some(12.0),
        // Battery / system rails that pull-ups legitimately tie to. Treated as a
        // rail for the "is a pull-up present" test (exact voltage immaterial
        // there); a nominal Li-ion midpoint is used for any current math.
        "VBAT" | "VBATT" | "VSYS" | "VIN" | "+VBAT" | "VPP" | "VDDIO" | "VDD_IO" | "VIO" => {
            Some(3.7)
        }
        _ => {
            if n.contains("5V") && (n.starts_with('+') || n.contains("VCC") || n.contains("VBUS")) {
                Some(5.0)
            } else if n.contains("3V3") || n.contains("3.3V") || n.contains("3P3") {
                Some(3.3)
            } else if n.contains("1V8") {
                Some(1.8)
            } else if n == "VBAT"
                || n.ends_with("/VBAT")
                || n.contains("VBAT")
                || n.contains("VSYS")
            {
                Some(3.7)
            } else {
                None
            }
        }
    }
}

/// Number of pads that carry a net (footprints sometimes add net-less paste /
/// mechanical pads, so `pins.len()` over-counts).
fn connected_pads(c: &Component) -> usize {
    c.pins.iter().filter(|p| p.net.is_some()).count()
}

/// Is this component a plain two-terminal resistor (the kind that can be a
/// pull-up)? Identified by ref designator + a chip-resistor footprint, and by
/// having exactly two *connected* pads (extra net-less pads in the footprint are
/// ignored, which is the bug that previously hid 0201 pull-ups).
fn is_resistor(c: &Component) -> bool {
    let r = c.reference.to_ascii_uppercase();
    let lib = c.lib_id.to_ascii_lowercase();
    // Exclude varistors (RV), thermistors (RT), and resistor networks (RN/RP/RM)
    // which are not plain two-terminal pulls.
    let is_r_ref = r.starts_with('R')
        && !r.starts_with("RV")
        && !r.starts_with("RT")
        && !r.starts_with("RN")
        && !r.starts_with("RP")
        && !r.starts_with("RM");
    is_r_ref && connected_pads(c) == 2 && !lib.contains("ferrite") && !lib.contains("inductor")
}

/// Is this an I2C level translator that provides its own bus pull-ups, so an
/// external pull-up on the connected bus is optional rather than missing?
///
/// Open-drain auto-direction translators (NXP NTS010x, TI TXS010x/TXB010x) and
/// buffered repeaters (PCA9517/PCA9617, LTC4311) integrate ~10 kOhm pull-ups to
/// each Vcc. A bare pass-gate switch (PCA9306, PCA9509 pass type) does NOT, so
/// it is deliberately excluded.
fn has_integrated_i2c_pullups(c: &Component) -> bool {
    let v = c.value.to_ascii_uppercase();
    const FAMILIES: [&str; 7] = [
        "NTS010", "TXS010", "TXB010", "PCA9517", "PCA9617", "LTC4311", "NVT2010",
    ];
    FAMILIES.iter().any(|f| v.contains(f))
}

/// Is this an LED (ref D* + value/lib hint, or explicit LED)?
fn is_led(c: &Component) -> bool {
    let lib = c.lib_id.to_ascii_lowercase();
    let val = c.value.to_ascii_lowercase();
    let r = c.reference.to_ascii_uppercase();
    lib.contains("led")
        || val.contains("led")
        || (r.starts_with("LED"))
        || (r.starts_with('D') && lib.contains(":led"))
}

/// True if the component looks like a bare connector / header / test point /
/// mounting hole - things whose pins legitimately dangle or break a bus out to
/// an external module that carries its own terminations.
///
/// Checks the reference prefix, the library/symbol id, AND the footprint /
/// package name, because boards label headers with non-obvious refs (Adafruit's
/// Arduino headers are `IOH`/`IOL`/`AD`/`POWER`, Eagle package `1X10`).
fn is_connector_like(c: &Component) -> bool {
    let r = c.reference.to_ascii_uppercase();
    let lib = c.lib_id.to_ascii_lowercase();
    let fp = c.footprint.to_ascii_lowercase();
    // Reference-prefix conventions.
    if r.starts_with('J')
        || r.starts_with("CN")
        || r.starts_with("CON")
        || r.starts_with("TP")
        || r.starts_with("MH")
        || r.starts_with("MK")
        || r.starts_with("SW") // a button is not a bus terminator
        || r == "POWER"
        || r.starts_with("IOH")
        || r.starts_with("IOL")
    {
        return true;
    }
    // P-prefixed pin headers (P1, PWR) but not transistors etc - P alone is
    // rare as a connector ref; keep it but guard against e.g. "PHY".
    if (r.starts_with('P') && r.len() <= 3 && r.chars().skip(1).all(|c| c.is_ascii_digit()))
        || r == "PWR"
    {
        return true;
    }
    // Library / symbol id.
    if lib.contains("connector")
        || lib.contains("conn:")
        || lib.contains("mountinghole")
        || lib.contains("testpoint")
        || lib.contains("header")
        || lib.contains("pinhead")
    {
        return true;
    }
    // Value string naming a connector / module interface (board-to-board, SoM,
    // SODIMM, FPC). Adapter / interposer boards label these with `U` refs.
    let val = c.value.to_ascii_lowercase();
    if val.contains("connector")
        || val.contains("-som")
        || val.contains("som_")
        || val.contains("sodimm")
        || val.contains("header")
        || val.contains("socket")
    {
        return true;
    }
    // Footprint / package geometry: pin rows, headers, sockets, board-to-board
    // and edge connectors (Hirose DF-series, SODIMM, FPC/FFC, card edge).
    fp.contains("header")
        || fp.contains("pinhead")
        || fp.contains("socket")
        || fp.contains("sodimm")
        || fp.contains("df40")
        || fp.contains("df12")
        || fp.contains("df9")
        || fp.contains("b2b")
        || fp.contains("fpc")
        || fp.contains("ffc")
        || fp.contains("edge")
        || fp.contains("_1x")
        || fp.contains("_2x")
        || is_pin_array_package(&c.footprint)
}

/// Eagle/KiCad pin-row package names like "1X10", "2X05", "1X10_OVALWAVE".
fn is_pin_array_package(fp: &str) -> bool {
    let f = fp.to_ascii_uppercase();
    // Look for a leading NxM token.
    for sep in ['X', 'x'] {
        if let Some(idx) = f.find(sep) {
            let before = &f[..idx];
            let after = &f[idx + 1..];
            let a_ok = !before.is_empty()
                && before
                    .chars()
                    .rev()
                    .take_while(|c| c.is_ascii_digit())
                    .count()
                    >= 1;
            let b_ok = after.chars().take_while(|c| c.is_ascii_digit()).count() >= 1;
            // require the digit immediately before X (e.g. "1X10")
            let imm = before
                .chars()
                .last()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false);
            if a_ok && b_ok && imm {
                return true;
            }
        }
    }
    false
}

/// All (component, pin) members of a net id.
fn members<'a>(board: &'a ExtractedBoard, net_id: i64) -> Vec<(&'a Component, &'a Pin)> {
    board.net_members(net_id)
}

/// Is this component a decoupling / bulk capacitor (ref C*, 2 connected pads)?
fn is_capacitor(c: &Component) -> bool {
    let r = c.reference.to_ascii_uppercase();
    r.starts_with('C') && !r.starts_with("CN") && !r.starts_with("CON") && connected_pads(c) == 2
}

fn passive_prefix(reference: &str) -> Option<char> {
    let r = reference.to_ascii_uppercase();
    let first = r.chars().next()?;
    match first {
        'R' if !r.starts_with("RV") && !r.starts_with("RT") && !r.starts_with("RN") => Some('R'),
        'C' if !r.starts_with("CN") && !r.starts_with("CON") => Some('C'),
        'L' => Some('L'),
        _ => None,
    }
}

fn footprint_family(footprint: &str) -> Option<char> {
    let fp = footprint.to_ascii_lowercase();
    if fp.contains("capacitor") || fp.contains(":c_") || fp.contains("/c_") {
        Some('C')
    } else if fp.contains("resistor") || fp.contains(":r_") || fp.contains("/r_") {
        Some('R')
    } else if fp.contains("inductor") || fp.contains(":l_") || fp.contains("/l_") {
        Some('L')
    } else {
        None
    }
}

fn package_code(footprint: &str) -> Option<&'static str> {
    let fp = footprint.to_ascii_lowercase();
    for code in ["0201", "0402", "0603", "0805", "1206", "1210"] {
        if fp.contains(code) {
            return Some(code);
        }
    }
    None
}

fn parse_capacitance_uf(value: &str) -> Option<f64> {
    let s = value.trim().replace('µ', "u").replace('μ', "u");
    if s.is_empty() {
        return None;
    }
    // Parse only the LEADING number+unit token. Collecting every digit in the
    // whole string mis-reads values that carry a voltage rating or a package
    // suffix: "10uF 25V" or "10u_0402" would otherwise concatenate to 1025 / 100402
    // uF and false-positive the package-ceiling check (a zero-false-positive
    // violation). Stop the number at the first non-numeric char, then take only
    // the immediately-following letters as the unit, ignoring any trailing text.
    let mut chars = s.chars().peekable();
    let mut num = String::new();
    while let Some(&ch) = chars.peek() {
        if ch.is_ascii_digit() || ch == '.' {
            num.push(ch);
            chars.next();
        } else {
            break;
        }
    }
    let n: f64 = num.parse().ok()?;
    let mut unit = String::new();
    while let Some(&ch) = chars.peek() {
        if ch.is_ascii_alphabetic() {
            unit.push(ch.to_ascii_lowercase());
            chars.next();
        } else {
            break;
        }
    }
    if unit.starts_with("pf") || unit == "p" {
        Some(n / 1_000_000.0)
    } else if unit.starts_with("nf") || unit == "n" {
        Some(n / 1000.0)
    } else if unit.starts_with("uf") || unit.starts_with('u') {
        Some(n)
    } else if unit.starts_with("mf") || unit == "m" {
        Some(n * 1000.0)
    } else {
        None
    }
}

fn mlcc_ceiling_uf(package: &str) -> Option<f64> {
    match package {
        "0201" => Some(1.0),
        "0402" => Some(22.0),
        "0603" => Some(100.0),
        "0805" => Some(220.0),
        _ => None,
    }
}

fn check_design_file_qc(board: &ExtractedBoard, report: &mut NetLintReport) {
    for c in &board.components {
        if c.dnp {
            continue;
        }
        let Some(prefix) = passive_prefix(&c.reference) else {
            continue;
        };
        let value_trimmed = c.value.trim();
        let value_upper = value_trimmed.to_ascii_uppercase();
        if value_trimmed.is_empty()
            || value_upper == "?"
            || (value_upper.len() == 1 && matches!(value_upper.as_str(), "R" | "C" | "L"))
        {
            report.findings.push(LintFinding {
                check: LintCheck::PlaceholderValue,
                severity: Severity::Medium,
                message: format!(
                    "{} has placeholder value '{}'; set the actual {} value before BOM/simulation",
                    c.reference, c.value, prefix
                ),
                refs: vec![c.reference.clone()],
                nets: Vec::new(),
            });
        }

        if let Some(family) = footprint_family(&c.footprint) {
            if family != prefix {
                report.findings.push(LintFinding {
                    check: LintCheck::DesignatorFootprintMismatch,
                    severity: Severity::Medium,
                    message: format!(
                        "{} is a {} designator but uses {} footprint '{}'",
                        c.reference, prefix, family, c.footprint
                    ),
                    refs: vec![c.reference.clone()],
                    nets: Vec::new(),
                });
            }
        }

        if prefix == 'C' {
            if let (Some(pkg), Some(uf)) =
                (package_code(&c.footprint), parse_capacitance_uf(&c.value))
            {
                if let Some(max_uf) = mlcc_ceiling_uf(pkg) {
                    if uf > max_uf {
                        report.findings.push(LintFinding {
                            check: LintCheck::ValuePackageSanity,
                            severity: Severity::Medium,
                            message: format!(
                                "{} value {} is implausible in {} (conservative ceiling {:.0}uF)",
                                c.reference, c.value, pkg, max_uf
                            ),
                            refs: vec![c.reference.clone()],
                            nets: Vec::new(),
                        });
                    }
                }
            }
        }
    }
}

/// A net is *rail-like* if its name is a recognised rail, OR it structurally
/// behaves like a local power node: it carries a bypass capacitor to ground
/// (the unmistakable signature of a supply) and is fed through a ferrite /
/// inductor or a regulator. This catches filtered / switched local rails that
/// CAD auto-names `Net-(C5-Pad2)` so a pull-up to them is not mis-flagged as
/// missing. A bus signal never carries a cap-to-ground, so this does not
/// misclassify SDA/SCL themselves.
fn net_is_raillike(board: &ExtractedBoard, net_id: i64) -> bool {
    if let Some(n) = board.net(net_id) {
        if rail_voltage(&n.name).is_some() {
            return true;
        }
    }
    let mem = members(board, net_id);
    // A bypass cap to ground on this net?
    let has_bypass_to_gnd = mem.iter().any(|(c, _)| {
        is_capacitor(c)
            && c.pins.iter().any(|op| {
                op.net
                    .filter(|id| *id != net_id)
                    .and_then(|id| board.net(id))
                    .map(|on| is_ground(&on.name))
                    .unwrap_or(false)
            })
    });
    has_bypass_to_gnd
}

/// KiCad emits one placeholder net per deliberately-unconnected pad, named
/// `unconnected-(REF-PIN-PadN)`. A pin on such a net is an explicit no-connect,
/// never a fault.
fn is_unconnected_net(name: &str) -> bool {
    name.trim_start_matches('/').starts_with("unconnected-")
}

/// The pin's electrical type marks it as a deliberate no-connect.
fn pin_is_no_connect(p: &Pin) -> bool {
    let k = p.kind.to_ascii_lowercase();
    k.contains("no_connect") || k == "nc" || k == "unconnected"
}

// ---------------------------------------------------------------------------
// Check 1: missing I2C pull-ups.
// ---------------------------------------------------------------------------

/// A net is an I2C data/clock line if its (leaf) name is exactly SDA/SCL or a
/// recognised decorated form (I2C_SDA, SDA1, ASDA, SDA_3V3, ...). We require the
/// SDA/SCL token to stand as its own word so we do not match "USDA" sub-strings
/// or unrelated nets.
fn i2c_role(name: &str) -> Option<&'static str> {
    let n = norm(name);
    // Split on common separators and look for an exact SDA/SCL token.
    let toks: Vec<&str> = n.split(|c: char| !c.is_ascii_alphanumeric()).collect();
    let has = |needle: &str| {
        toks.iter().any(|t| {
            // exact, or token like "SDA1"/"SDA2" (the bus index suffix), or
            // "ASDA"/"ASCL" (Watchy's alt I2C). Strip a single leading 'A' and a
            // trailing digit before comparing.
            let t = t.strip_prefix('A').unwrap_or(t);
            let t = t.trim_end_matches(|c: char| c.is_ascii_digit());
            t == needle
        })
    };
    if has("SDA") {
        Some("SDA")
    } else if has("SCL") {
        Some("SCL")
    } else {
        None
    }
}

fn check_i2c_pullups(board: &ExtractedBoard, report: &mut NetLintReport) {
    for net in &board.nets {
        if net.id == 0 || is_unconnected_net(&net.name) {
            continue;
        }
        let Some(role) = i2c_role(&net.name) else {
            continue;
        };
        let mem = members(board, net.id);
        // A real two-wire bus has at least two members (master + peripheral, or
        // a peripheral + the pull). A single-member "SDA" is a stub / NC pad.
        if mem.len() < 2 {
            continue;
        }

        // A pull-up is a resistor with one pad on this net and the other pad on
        // a power rail (not ground). Look for it.
        let mut has_pullup = false;
        let mut active_devices = 0usize;
        let mut exits_to_connector = false;

        for (c, _p) in &mem {
            // A level translator with integrated pull-ups (NTS010x / TXS010x /
            // TXB010x / PCA9517-class auto-direction parts) supplies the bus
            // pull-up itself: an external resistor is optional, not missing. A
            // pure pass-gate (PCA9306) does NOT, so it is excluded from this.
            if has_integrated_i2c_pullups(c) {
                has_pullup = true;
            }
            if is_connector_like(c) {
                exits_to_connector = true;
                continue;
            }
            // An IC / sensor pin on the bus means the bus is used on-board.
            let r = c.reference.to_ascii_uppercase();
            if r.starts_with('U') || (c.pins.len() > 2 && !is_resistor(c)) {
                active_devices += 1;
            }
            if is_resistor(c) {
                // Does the other pad land on a rail (named, or a structural
                // local rail with a bypass cap to ground)?
                for op in &c.pins {
                    if op.net == Some(net.id) {
                        continue;
                    }
                    if let Some(oid) = op.net {
                        if net_is_raillike(board, oid) {
                            has_pullup = true;
                        }
                    }
                }
            }
        }

        if has_pullup {
            continue;
        }

        // Two regimes:
        //  - The bus exits to a header / connector with no on-board pull. This is
        //    the *intentional* "pull-ups live on the attached module" pattern
        //    (every Arduino/Feather breaks I2C out to pins this way). Low / known.
        //  - The bus is entirely on-board (a master and at least one on-board
        //    peripheral) yet has no pull-up anywhere. This is the higher-
        //    confidence "genuinely missing" case. Medium.
        let (sev, note) = if exits_to_connector {
            (
                Severity::Low,
                "bus breaks out to a header; pull-ups conventionally on the attached module",
            )
        } else if active_devices >= 2 {
            (
                Severity::Medium,
                "on-board master and peripheral, no pull-up present",
            )
        } else {
            // A single on-board device with no header and no pull: ambiguous,
            // skip rather than risk a false positive.
            continue;
        };

        report.findings.push(LintFinding {
            check: LintCheck::MissingI2cPullup,
            severity: sev,
            message: format!(
                "I2C {role} net '{}' has no pull-up to a rail: {note}",
                net.name
            ),
            refs: mem.iter().map(|(c, _)| c.reference.clone()).collect(),
            nets: vec![net.name.clone()],
        });
    }
}

// ---------------------------------------------------------------------------
// Check 2: floating active-control pins.
// ---------------------------------------------------------------------------

/// Does this pin function name denote a *dedicated* active-control input whose
/// level must be defined at power-up (a chip enable, reset, shutdown, ...)?
///
/// Deliberately strict to avoid matching multiplexed GPIO signal names that
/// merely contain "EN"/"CS" as a substring (e.g. an Ethernet RMII `TX_EN`
/// strobe, a `GPIO21/EMAC_TX_EN` mux pin, a `SENSE` line, an `OPEN` flag). We
/// match only when the function name *is* a recognised control pin, after
/// stripping active-low decoration. A name carrying a peripheral/mux prefix
/// (containing '/', or a known bus/peripheral keyword) is treated as a signal,
/// not a control pin.
fn control_role(function: &str) -> Option<&'static str> {
    let f = function.trim().to_ascii_uppercase();
    if f.is_empty() {
        return None;
    }
    // A multiplexed / signal name (Ethernet, SPI, GPIO mux, etc.) is not a
    // dedicated control pin even if a sub-token looks like "EN".
    const SIGNAL_KEYWORDS: [&str; 12] = [
        "GPIO", "EMAC", "RMII", "TX_EN", "RX_EN", "CLKEN", "VSPI", "HSPI", "UART", "PWM", "SENSE",
        "OPEN",
    ];
    if SIGNAL_KEYWORDS.iter().any(|k| f.contains(k)) {
        return None;
    }
    // A '/'-bearing name is a mux alias list ("D2/A1/SCL"); not a control pin.
    if f.contains('/') && !f.starts_with('/') {
        return None;
    }
    // Strip active-low decoration: ~{RST}, /RESET, RST#, nRST, RST_N, RESETN.
    let core: String = f
        .trim_start_matches('~')
        .trim_start_matches('/')
        .replace(['~', '{', '}', '/', '#'], "");
    // Strip a leading 'N' (nRST) and trailing '_N'/'N' active-low suffix.
    let core = core.trim_start_matches('N').to_string();
    let core = core.trim_end_matches("_N").to_string();
    let c = core.trim();

    // Exact / canonical matches only.
    match c {
        "EN" | "ENABLE" | "CE" | "CEN" | "SHDN" | "SHUTDOWN" | "NSHDN" => Some("enable"),
        "RST" | "RESET" | "MR" | "RESE" => Some("reset"), // RESE = RESET after _N strip
        "CS" | "SS" | "NCS" | "NSS" | "CSB" => Some("chip-select"),
        "OE" | "NOE" => Some("output-enable"),
        _ => {
            // Allow a trailing rail-name suffix on EN/RST: "EN_3V3", "RST_MCU".
            let head = c.split('_').next().unwrap_or(c);
            match head {
                "EN" => Some("enable"),
                "RST" | "RESET" => Some("reset"),
                _ => None,
            }
        }
    }
}

fn check_floating_control_pins(board: &ExtractedBoard, report: &mut NetLintReport) {
    for c in &board.components {
        // Only logic/active parts; skip connectors/passives where "EN" pin
        // names are meaningless.
        if is_connector_like(c) {
            continue;
        }
        let r = c.reference.to_ascii_uppercase();
        let active = r.starts_with('U') || r.starts_with("IC") || r.starts_with('Q');
        if !active {
            continue;
        }
        for p in &c.pins {
            let Some(role) = control_role(&p.function) else {
                continue;
            };
            // A deliberate no-connect (by pintype or by an `unconnected-(...)`
            // placeholder net) is the designer's explicit choice, not a fault.
            if pin_is_no_connect(p) {
                continue;
            }
            let net = p.net.and_then(|id| board.net(id));
            if let Some(n) = net {
                if is_unconnected_net(&n.name) {
                    continue;
                }
            }
            // Floating == the pin's net has degree 1 (only this pin). A control
            // pin tied to a rail, a pull, or any driver has degree >= 2. We
            // require a *named* net of degree 1 (an explicitly-drawn stub that
            // goes nowhere), which is the high-confidence floating signature;
            // a None net is usually an unrouted pad the layout will fill, so we
            // do not fire on it (too many false positives on PCB-only inputs).
            let Some(n) = net else { continue };
            let degree = p.net.map(|id| members(board, id).len()).unwrap_or(0);
            if degree == 1 {
                report.findings.push(LintFinding {
                    check: LintCheck::FloatingControlPin,
                    severity: Severity::High,
                    message: format!(
                        "{} pin {} ({}) is a floating {role} input: net '{}' touches only this pin (no driver or pull)",
                        c.reference, p.number, p.function, n.name
                    ),
                    refs: vec![c.reference.clone()],
                    nets: vec![n.name.clone()],
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Check 3: LED series-resistor current sanity.
// ---------------------------------------------------------------------------

/// Parse a resistor value string ("330", "1k", "4k7", "1.2K", "0R") to ohms.
fn parse_ohms(v: &str) -> Option<f64> {
    let s = v.trim().to_ascii_uppercase();
    let s = s
        .trim_end_matches('Ω')
        .trim_end_matches("OHM")
        .trim_end_matches('R');
    // Handle "4K7" style (R/K/M as decimal point).
    for (suffix, mult) in [("K", 1e3), ("M", 1e6), ("R", 1.0)] {
        if let Some(idx) = s.find(suffix) {
            let (a, b) = s.split_at(idx);
            let b = &b[suffix.len()..];
            // Leading-suffix sub-1-ohm notation ("R47" = 0.47 Ω) leaves the
            // integer part empty; treat it as 0 rather than failing the parse
            // (which would abort the whole resistor search via `?` and silently
            // skip the LED-current check on a near-dead-short).
            let a: f64 = if a.is_empty() { 0.0 } else { a.parse().ok()? };
            if b.is_empty() {
                return Some(a * mult);
            }
            let frac: f64 = format!("0.{b}").parse().ok()?;
            return Some((a + frac) * mult);
        }
    }
    s.parse().ok()
}

/// Typical LED forward voltage; we use a conservative single value since colour
/// is rarely encoded. 2.0 V splits red (1.8) and green/blue (2.8..3.2); the
/// check bands are wide enough that the exact Vf does not produce false fires.
const LED_VF: f64 = 2.0;
/// Indicator LED current sanity band (A). Below: too dim to matter (not a
/// finding). Above HIGH: stressing the LED / wasting power.
const LED_I_MAX_OK: f64 = 0.030; // 30 mA: typical indicator absolute-ish ceiling

fn check_led_current(board: &ExtractedBoard, report: &mut NetLintReport) {
    for led in &board.components {
        if !is_led(led) || led.pins.len() != 2 {
            continue;
        }
        // Find the two nets of the LED.
        let mut led_nets = [None, None];
        for (i, p) in led.pins.iter().enumerate().take(2) {
            led_nets[i] = p.net;
        }
        let (Some(na), Some(nb)) = (led_nets[0], led_nets[1]) else {
            continue;
        };

        // One side should ultimately reach a rail through a series resistor and
        // the other side a return (ground or a driven low). We look for a
        // resistor sharing one of the LED's nets whose far pad is a rail, and
        // require the LED's other net to be ground (the classic always-on /
        // indicator topology). GPIO-driven LEDs (far side an MCU pin) are not
        // flagged: the firmware sets the level and the resistor still bounds the
        // current, so the static current is the worst case either way - but to
        // stay conservative we only fire when one terminal is a hard rail and
        // the other a hard ground.
        for (anode_net, cathode_net) in [(na, nb), (nb, na)] {
            // series resistor on the anode side reaching a rail
            let rail_v = resistor_to_rail(board, anode_net, led.reference.as_str());
            let cathode_is_gnd = board
                .net(cathode_net)
                .map(|n| is_ground(&n.name))
                .unwrap_or(false);
            if let Some((rref, ohms, vrail)) = rail_v {
                if cathode_is_gnd && ohms > 0.0 {
                    let i = (vrail - LED_VF) / ohms;
                    if i > LED_I_MAX_OK {
                        report.findings.push(LintFinding {
                            check: LintCheck::LedCurrentSanity,
                            severity: Severity::Low,
                            message: format!(
                                "{} via {} ({:.0}Ω) from {:.1}V rail draws ~{:.1} mA (>{:.0} mA)",
                                led.reference,
                                rref,
                                ohms,
                                vrail,
                                i * 1e3,
                                LED_I_MAX_OK * 1e3
                            ),
                            refs: vec![led.reference.clone(), rref],
                            nets: vec![],
                        });
                    }
                    break;
                }
            }
        }
    }
}

/// If a resistor has one pad on `net` and the other on a power rail, return
/// (resistor ref, ohms, rail voltage).
fn resistor_to_rail(
    board: &ExtractedBoard,
    net: i64,
    _exclude: &str,
) -> Option<(String, f64, f64)> {
    for (c, _p) in members(board, net) {
        if !is_resistor(c) {
            continue;
        }
        let ohms = parse_ohms(&c.value)?;
        for op in &c.pins {
            if op.net == Some(net) {
                continue;
            }
            if let Some(oid) = op.net {
                if let Some(on) = board.net(oid) {
                    if let Some(v) = rail_voltage(&on.name) {
                        return Some((c.reference.clone(), ohms, v));
                    }
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Check 4: output-vs-output contention (schematic-stage ERC).
// ---------------------------------------------------------------------------
//
// Two distinct parts each driving the same net with a *push-pull* output, with
// nothing on the net that can resolve the fight (no series resistor, no
// tri-state/open-drain/bidirectional pin, no input that reframes the net as a
// driven-input), is a wiring error: at any instant the two outputs can command
// opposite levels and source/sink destructive cross-current.
//
// This is built purely from the KiCad pin electrical type (`Pin.kind`), which
// only the schematic / netlist extraction paths populate. It is calibrated to be
// SILENT on the full known-good schematic corpus (ZSWatch x4, Watchy, LumenPnP
// x2, Olimex EVB, Corne, Lily58, RP2040-minimal, Reform x2, Olimex Pico-PC):
// the single raw fire on that corpus (Reform `EDP_IRQ`) is an open-drain
// interrupt line whose pins a symbol author typed `output`, and it is excluded
// by the open-drain-name and tiebreaker rules below. See `docs/record/FAMOUS_SWEEP.md`
// Round 4 for the calibration evidence and for why the sibling "undriven input"
// check was REJECTED (it could not reach zero false positives).

/// A push-pull driver type. `tri_state` / `open_collector` / `open_emitter` are
/// deliberately NOT here: those are wired-OR safe and resolve contention.
fn is_pushpull_output(kind: &str) -> bool {
    matches!(
        kind.trim().to_ascii_lowercase().as_str(),
        "output" | "power_out"
    )
}

/// A member that can legitimately resolve two outputs sharing a net: a passive
/// (series R / mux), a tri-state / open-drain / open-emitter pin, or an input/
/// bidirectional pin (which reframes the net as a driven input the symbol author
/// modelled as also-output, the common IRQ / shared-IO case). Its presence means
/// "do not fire" - we only flag a net whose ONLY drivers are bare push-pull
/// outputs of different parts.
fn resolves_contention(kind: &str) -> bool {
    matches!(
        kind.trim().to_ascii_lowercase().as_str(),
        "passive" | "tri_state" | "open_collector" | "open_emitter" | "input" | "bidirectional"
    )
}

/// A net whose name marks it as an open-drain / wired-OR line (interrupt, alert,
/// ready/busy, NMI). Symbol authors routinely type these pins `output` though
/// they are open-drain pulled high; two such pins on one net is the intended
/// wired-OR, not contention. Excluded by name so the check stays at zero false
/// positives.
fn is_wired_or_name(name: &str) -> bool {
    let n = norm(name);
    [
        "IRQ", "INT", "ALERT", "RDY", "READY", "BUSY", "NMI", "PG", "PGOOD", "FAULT", "NFLT",
    ]
    .iter()
    .any(|k| n.contains(k))
}

fn check_output_contention(board: &ExtractedBoard, report: &mut NetLintReport) {
    for net in &board.nets {
        if net.id == 0 || is_unconnected_net(&net.name) || is_ground(&net.name) {
            continue;
        }
        // Wired-OR / interrupt nets carry multiple open-drain "outputs" by design.
        if is_wired_or_name(&net.name) {
            continue;
        }
        let mem = members(board, net.id);

        // Collect push-pull output pins on non-connector parts. A connector pin
        // is an off-board interface, never an on-board push-pull driver.
        let mut drivers: Vec<(&Component, &Pin)> = Vec::new();
        let mut any_resolver = false;
        for (c, p) in &mem {
            if is_pushpull_output(&p.kind) && !is_connector_like(c) {
                drivers.push((c, p));
            }
            if resolves_contention(&p.kind)
                || c.reference.to_ascii_uppercase().starts_with('R')
                || is_connector_like(c)
            {
                any_resolver = true;
            }
        }
        // Need >= 2 *distinct* driver parts (two pins of one chip on a net is an
        // internal short the symbol expresses, not an inter-part fight).
        let distinct: std::collections::HashSet<&str> =
            drivers.iter().map(|(c, _)| c.reference.as_str()).collect();
        if distinct.len() < 2 {
            continue;
        }
        // Any resolver on the net (series R, tri-state, input, connector) means
        // the contention is or can be resolved: do not fire.
        if any_resolver {
            continue;
        }

        // Severity: if two power_out pins of *different* nominal voltage are tied
        // (a hard rail-vs-rail short), that is High. Otherwise (two signal
        // outputs) Medium.
        let pwr_voltages: Vec<f64> = drivers
            .iter()
            .filter(|(_, p)| p.kind.eq_ignore_ascii_case("power_out"))
            .filter_map(|_| rail_voltage(&net.name))
            .collect();
        let _ = pwr_voltages; // net name rarely encodes the source rail; kept for clarity
        let power_out = drivers
            .iter()
            .filter(|(_, p)| p.kind.eq_ignore_ascii_case("power_out"))
            .count();
        let sev = if power_out >= 2 {
            Severity::High
        } else {
            Severity::Medium
        };

        let drv_desc: Vec<String> = drivers
            .iter()
            .map(|(c, p)| format!("{}.{} ({})", c.reference, p.number, p.kind))
            .collect();
        report.findings.push(LintFinding {
            check: LintCheck::OutputContention,
            severity: sev,
            message: format!(
                "net '{}' is driven by {} push-pull outputs with no series resistor or tri-state to resolve them: {}",
                net.name,
                drivers.len(),
                drv_desc.join(", ")
            ),
            refs: distinct.iter().map(|s| s.to_string()).collect(),
            nets: vec![net.name.clone()],
        });
    }
}

/// Render a lint report as a Unicode table.
pub fn render_netlint(report: &NetLintReport) -> String {
    let mut out = String::new();
    if report.findings.is_empty() {
        out.push_str("net-lint: no findings.\n");
        return out;
    }
    out.push_str(&format!("net-lint: {} finding(s)\n", report.findings.len()));
    for f in &report.findings {
        out.push_str(&format!(
            "  [{}] {} - {}\n",
            f.severity.as_str(),
            f.check.as_str(),
            f.message
        ));
    }
    out
}

#[cfg(test)]
mod parse_ohms_tests {
    use super::parse_ohms;

    #[test]
    fn leading_r_sub_ohm_notation_parses() {
        // R5 regression: "R47" = 0.47 Ω (the leading-R sub-1-ohm marking). The
        // empty integer part used to fail the parse and, via `?`, abort the
        // whole rail-resistor search — silently skipping the LED-current check
        // on a near-dead-short.
        assert_eq!(parse_ohms("R47"), Some(0.47));
        assert_eq!(parse_ohms("r47"), Some(0.47));
        // The forms that already worked must still work.
        assert_eq!(parse_ohms("4R7"), Some(4.7));
        assert_eq!(parse_ohms("47R"), Some(47.0));
        assert_eq!(parse_ohms("0R"), Some(0.0));
        assert_eq!(parse_ohms("4K7"), Some(4700.0));
        assert_eq!(parse_ohms("330"), Some(330.0));
    }
}
