//! MCU internal resource-conflict check.
//!
//! The class: two board-level functions are routed by the netlist to *different*
//! MCU pins, and the wiring looks fine, but the two pins map to the *same shared
//! silicon resource instance* inside the MCU - one PWM slice+channel, one QSPI
//! pin group, one SERCOM, one timer channel - so the MCU physically cannot serve
//! both functions at once. This is invisible to a connectivity sweep (no short,
//! no missing pull, no contention on the board); it only shows up when you know
//! the MCU's internal peripheral-to-pin binding.
//!
//! Two real, shipped boards define and validate the check:
//!
//! 1. **Olimex RP2040-PICO-PC**: the PicoDVI clock uses PWM on GP12/GP13 while
//!    the board's PWM stereo audio sits on GP27/GP28. GP12 and GP28 both map to
//!    RP2040 **PWM slice 6, channel A** (the (n>>1)&7 / A|B rule), so DVI and
//!    audio cannot have independent PWM and in fact want the *same* channel.
//!
//! 2. **SparkFun SAMD51 Thing Plus**: the on-board SPI flash is wired to
//!    PA08..PA11, the SAM D5x **QSPI DATA0..3** pins, with SCK/CS off the
//!    QSPI-locked PB10/PB11, so the QSPI peripheral can never drive it and only
//!    SERCOM SPI can. Vendor firmware declares plain SPI flash on exactly these
//!    pins and the board works as designed, which is why single-function group
//!    occupation is a low-severity fact rather than a conflict. Two DIFFERENT
//!    functions on one group stays serious.
//!
//! The MCU resource map lives in `db/mcu_resources.toml` (hand-authored from the
//! reference manuals, cited there). The function each used pin is demanded for
//! is inferred *conservatively* from what the pin's net connects to (an HDMI/DVI
//! connector, an audio jack, a flash chip), and the evidence is carried in the
//! finding so it can be chased to the file. A conflict is reported only when two
//! *distinct* board functions provably demand the same resource instance.

use std::collections::BTreeMap;

use hauksbee_models::netclass;
use hauksbee_models::refdes::RefClass;
use regex::Regex;

use crate::netlint::{LintCheck, LintFinding, NetLintReport, Severity};
use crate::{Component, ExtractedBoard};

// ---------------------------------------------------------------------------
// The resource map (parsed once from the embedded TOML).
// ---------------------------------------------------------------------------

/// The hand-authored per-MCU resource table, embedded at build time.
const MCU_RESOURCES_TOML: &str = include_str!("../db/mcu_resources.toml");

/// One pin's internal-resource bindings: the resource instances this pad is
/// hardwired (or, for muxed groups, committed) to.
#[derive(Debug, Clone, Default)]
struct PinResources {
    /// Friendly name for the pad (the GPIO / port name), for the evidence chain.
    label: String,
    /// PWM slice+channel id like "6A" (RP2040). One pad -> one PWM channel.
    pwm: Option<String>,
    /// QSPI pin-group id like "`qspi_data`" (SAMD51): a *group* of pads that the
    /// QSPI controller owns together.
    qspi_group: Option<String>,
    /// True for a QSPI DATA pad (PA08..PA11): the discriminator pads. A 4-wire
    /// SERCOM-SPI signal landing here proves the bug; a quad-IO data signal here
    /// is a correct QSPI flash.
    qspi_data_pad: bool,
}

/// One MCU's resource table.
#[derive(Debug, Clone)]
struct McuResources {
    id: String,
    value_re: Regex,
    lib_re: Option<Regex>,
    /// True for an MCU whose digital peripherals are fully routable (ESP32): it
    /// has no fixed pin->instance conflicts in this class, so the check must
    /// never manufacture one for it.
    fully_routable: bool,
    /// When set, the MCU has a software-programmable engine that can drive ANY
    /// GPIO (RP2040 PIO), so a hardware-PWM channel collision is a firmware
    /// constraint, not a board defect: one of the two functions can be generated
    /// by that engine instead. The text (with its datasheet citation) is quoted
    /// in the finding, which is then a warning rather than serious.
    pwm_software_fallback: Option<String>,
    /// Minimum connected-pin count for a component to count as this MCU/module
    /// (guards against loose name matches on small parts). Optional.
    min_pins: Option<usize>,
    /// pad number -> resources.
    pins: BTreeMap<String, PinResources>,
}

impl McuResources {
    fn matches(&self, c: &Component) -> bool {
        // The part must (a) name-match by value or lib_id, AND (b) be physically
        // big enough to be the MCU/module this table describes. The pin-count
        // guard is load-bearing: without it the loose "PICO" substring in a
        // KiCad rescue-library id (`...RP2040-PICO-PC_rev_C-rescue:74LVC125...`)
        // would match a 14-pin buffer and a 1-pin mounting hole. The table's
        // highest pad number is the lower bound on a real instance's pin count.
        let name_hit = self.value_re.is_match(&c.value)
            || self
                .lib_re
                .as_ref()
                .is_some_and(|re| re.is_match(&c.lib_id));
        if !name_hit {
            return false;
        }
        if let Some(min_pins) = self.min_pins {
            if c.pins.iter().filter(|p| p.net.is_some()).count() < min_pins {
                return false;
            }
        }
        true
    }
}

/// Parse the embedded resource TOML into the in-memory tables. Done lazily once.
fn load_resources() -> Vec<McuResources> {
    parse_resources(MCU_RESOURCES_TOML).expect("embedded mcu_resources.toml must parse")
}

fn parse_resources(text: &str) -> Result<Vec<McuResources>, String> {
    let doc: toml::Value = toml::from_str(text).map_err(|e| e.to_string())?;
    let arr = doc
        .get("mcu")
        .and_then(|m| m.as_array())
        .ok_or("mcu_resources.toml: missing [[mcu]] array")?;
    let mut out = Vec::new();
    for entry in arr {
        let id = entry
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or("[[mcu]] missing id")?
            .to_string();
        let m = entry.get("match").ok_or("[[mcu]] missing [mcu.match]")?;
        let value_re = Regex::new(
            m.get("value_re")
                .and_then(|v| v.as_str())
                .ok_or("[mcu.match] missing value_re")?,
        )
        .map_err(|e| format!("{id}: bad value_re: {e}"))?;
        let lib_re = match m.get("lib_re").and_then(|v| v.as_str()) {
            Some(s) => Some(Regex::new(s).map_err(|e| format!("{id}: bad lib_re: {e}"))?),
            None => None,
        };
        let fully_routable = entry
            .get("fully_routable")
            .and_then(toml::Value::as_bool)
            .unwrap_or(false);
        let pwm_software_fallback = entry
            .get("pwm_software_fallback")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let min_pins = entry
            .get("min_pins")
            .and_then(toml::Value::as_integer)
            .and_then(|n| usize::try_from(n).ok());
        let mut pins = BTreeMap::new();
        if let Some(table) = entry.get("pins").and_then(|p| p.as_table()) {
            for (pad, v) in table {
                let label = v
                    .get("gpio")
                    .or_else(|| v.get("port"))
                    .and_then(|x| x.as_str())
                    .unwrap_or(pad)
                    .to_string();
                let pwm = v.get("pwm").and_then(|x| x.as_str()).map(str::to_string);
                let qspi_group = v.get("group").and_then(|x| x.as_str()).map(str::to_string);
                let qspi_data_pad = v
                    .get("data")
                    .and_then(toml::Value::as_bool)
                    .unwrap_or(false);
                pins.insert(
                    pad.clone(),
                    PinResources {
                        label,
                        pwm,
                        qspi_group,
                        qspi_data_pad,
                    },
                );
            }
        }
        out.push(McuResources {
            id,
            value_re,
            lib_re,
            fully_routable,
            pwm_software_fallback,
            min_pins,
            pins,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Board-level function inference (conservative, evidence-recording).
// ---------------------------------------------------------------------------

/// A board-level function we have evidence an MCU pin is being USED for, with
/// the evidence chain that justifies the claim.
#[derive(Debug, Clone)]
struct PinDemand {
    /// MCU pad number.
    pad: String,
    /// The pad's friendly label (GPIO / port name) from the resource table.
    label: String,
    /// The function class demanded (e.g. "DVI/TMDS link", "PWM audio",
    /// "SPI flash"). Two demands with *different* `function` on one shared
    /// resource instance are the conflict.
    function: String,
    /// Net name the pad is on (the start of the evidence chain).
    net: String,
    /// Human-readable evidence: what on that net implies the function.
    evidence: String,
    /// The resource instance this demand occupies, e.g. ("pwm", "6A") or
    /// ("qspi", "`qspi_data`").
    resource_kind: &'static str,
    resource_inst: String,
}

/// Which MCU peripheral a demanded function uses. This is the load-bearing
/// distinction for the PWM-slice check: a PWM slice is only contended by two
/// functions that BOTH use the PWM peripheral. A DVI link reaching the HDMI
/// connector is *not* automatically a PWM demand - on PicoDVI only the pixel/bit
/// CLOCK is generated with PWM, while the three TMDS data lanes are driven by
/// PIO+DMA and the DDC/CEC control lines are I2C. Conflating them would
/// manufacture false PWM conflicts on the data and control pins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Peripheral {
    /// Uses a PWM slice/channel (RP2040 PWM audio; the PicoDVI PWM clock).
    Pwm,
    /// A SERCOM/SPI-style serial link to a peripheral (flash, etc).
    SpiLike,
    /// Anything else that reaches a target but does NOT contend for the resource
    /// instances this check models (TMDS-data over PIO, I2C DDC, plain GPIO).
    Other,
}

/// The board-level function a used MCU pin is demanded for: a human-readable
/// class plus the MCU peripheral it uses.
#[derive(Debug, Clone)]
struct Function {
    class: String,
    peripheral: Peripheral,
}

/// What target does the far end of a used MCU pin reach? Narrow and unambiguous:
/// only a display connector, an audio jack, or a flash chip. The *peripheral* is
/// then decided by combining this target with the MCU pin's own net name (the
/// `start_net` argument), so a clock net to HDMI is PWM while a data net to HDMI
/// is not.
fn classify_target(c: &Component) -> Option<&'static str> {
    let v = c.value.to_ascii_lowercase();
    let lib = c.lib_id.to_ascii_lowercase();
    let fp = c.footprint.to_ascii_lowercase();
    let r = c.reference.to_ascii_uppercase();
    let any = |hay: &str, needles: &[&str]| needles.iter().any(|n| hay.contains(n));

    if any(&v, &["hdmi", "dvi"])
        || any(&lib, &["hdmi", "dvi"])
        || any(&fp, &["hdmi", "dvi"])
        || r.starts_with("HDMI")
        || r.starts_with("DVI")
    {
        return Some("display");
    }
    if any(&v, &["audio", "headphone", "phone jack", "3.5mm", "pj-"])
        || any(&lib, &["audio", "jack"])
        || r.starts_with("AUDIO")
        || r.contains("JACK")
    {
        return Some("audio");
    }
    if any(
        &v,
        &[
            "flash",
            "25sf",
            "25q",
            "w25",
            "at25",
            "mx25",
            "spansion",
            "nor flash",
        ],
    ) || any(&lib, &["memory", "flash"])
    {
        return Some("flash");
    }
    None
}

/// Decide the (class, peripheral) for a demand, given the target reached and the
/// MCU pin's own net name. The net name is the honest discriminator for which
/// signal of a multi-function connector this pin carries.
fn function_for(target: &str, start_net: &str) -> Option<Function> {
    let n = start_net
        .trim()
        .rsplit('/')
        .next()
        .unwrap_or(start_net)
        .to_ascii_uppercase();
    let has = |needles: &[&str]| needles.iter().any(|k| n.contains(k));
    match target {
        "display" => {
            // The PicoDVI pixel/bit CLOCK is the only PWM-generated DVI signal
            // (net named CK / CLK / PIXCLK). The TMDS data lanes (D0..D2) are
            // PIO-driven; DDC/CEC/HPD are I2C/GPIO. Only the clock is a PWM
            // demand.
            if has(&["CK", "CLK", "PIXCLK", "PIX_CLK"]) && !has(&["CLKEN"]) {
                Some(Function {
                    class: "PicoDVI PWM pixel clock".into(),
                    peripheral: Peripheral::Pwm,
                })
            } else {
                Some(Function {
                    class: "DVI link (non-PWM)".into(),
                    peripheral: Peripheral::Other,
                })
            }
        }
        "audio" => {
            // The pin reaches an audio jack, and because `infer_target` only
            // crosses series passives plus a small line buffer it does so through
            // an RC reconstruction-filter path, not an active codec/DAC (a codec
            // is not a series bridge, so such a path would not resolve here). On
            // an RP2040, which has no DAC and no hardware I2S, that path IS PWM
            // audio whatever the net is named; the buffer+RC-to-jack topology is
            // the evidence.
            //
            // A jack control/sense line (headphone-detect, insertion, sense) is
            // excluded: it reaches the jack but carries no audio.
            if has(&[
                "DET", "SENSE", "SENS", "INS", "INSERT", "HPDET", "JACK_DET", "MIC_DET",
            ]) {
                return Some(Function {
                    class: "jack sense (non-PWM)".into(),
                    peripheral: Peripheral::Other,
                });
            }
            Some(Function {
                class: "PWM audio".into(),
                peripheral: Peripheral::Pwm,
            })
        }
        "flash" => Some(Function {
            class: "SPI flash".into(),
            peripheral: Peripheral::SpiLike,
        }),
        _ => None,
    }
}

/// Does this net name carry a 4-wire (single-data-lane) SPI signal role -
/// MOSI / MISO / SCK / CS / SI / SO / SDI / SDO - as opposed to a quad-IO data
/// lane (IO0..IO3 / D0..D3 / DAT0..3 / SD0..3)? This is the discriminator
/// between the SparkFun SERCOM-SPI-on-QSPI-pins bug (4-wire SPI naming on the
/// QSPI data pads) and a correctly-wired QSPI flash (quad-IO naming). Returns
/// true ONLY for an unambiguous 4-wire-SPI role and false for quad-IO or
/// anything else, so a flash net we cannot read does not fire.
fn net_role_is_4wire_spi(name: &str) -> bool {
    let n = name
        .trim()
        .rsplit('/')
        .next()
        .unwrap_or(name)
        .to_ascii_uppercase();
    // Tokenise on non-alphanumerics so "FLASH_MOSI" -> ["FLASH","MOSI"].
    let toks: Vec<&str> = n
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    // A quad-IO data lane is NOT a 4-wire SPI role (it is correct QSPI).
    let is_quad_io = toks.iter().any(|t| {
        matches!(
            *t,
            "IO0"
                | "IO1"
                | "IO2"
                | "IO3"
                | "DAT0"
                | "DAT1"
                | "DAT2"
                | "DAT3"
                | "SD0"
                | "SD1"
                | "SD2"
                | "SD3"
                | "D0"
                | "D1"
                | "D2"
                | "D3"
                | "QSPI"
                | "QIO"
        )
    });
    if is_quad_io {
        return false;
    }
    toks.iter().any(|t| {
        matches!(
            *t,
            "MOSI"
                | "MISO"
                | "SCK"
                | "SCLK"
                | "CS"
                | "NCS"
                | "SSEL"
                | "SS"
                | "SI"
                | "SO"
                | "SDI"
                | "SDO"
                | "COPI"
                | "CIPO"
                | "CLK"
        )
    })
}

/// Is this net a power rail or ground? The inference must NEVER traverse such a
/// net: a rail/ground touches almost every part, so following it would make
/// every MCU pin "reach" every connector. The one net table
/// (`hauksbee_models::netclass`), so `5V_EN` or `PWR_3V3_OK` is a signal here
/// exactly as it is in netlint.
fn is_rail_or_ground(name: &str) -> bool {
    netclass::is_ground(name) || netclass::is_supply(name)
}

/// Walk the signal net a used MCU pin is on and, if a single unambiguous
/// downstream function is implied, return (function, evidence). The walk follows
/// the SIGNAL path only: it never crosses a power/ground net and never re-enters
/// an MCU, so it cannot wander off the intended trace. It hops through genuine
/// two-terminal series elements (a TMDS series-termination resistor, a PWM-audio
/// RC reconstruction filter) and a small line buffer, skipping pads that land on
/// rails. Conservative: no classifiable part on the signal path -> None.
/// Returns (target-kind, evidence-chain) where target-kind is one of "display",
/// "audio", "flash" (the strings `classify_target` yields).
fn infer_target(
    board: &ExtractedBoard,
    is_mcu: &dyn Fn(&Component) -> bool,
    net_id: i64,
    depth: u8,
    seen: &mut Vec<i64>,
) -> Option<(&'static str, String)> {
    // Depth 6 lets the walk cross a multi-pole RC reconstruction filter (the
    // PWM-audio path on the Olimex board runs GPIO -> 74LVC125 -> a 3..4-stage
    // R/C ladder -> jack), while the series-bridge-only + rail-skip + no-MCU
    // constraints keep it from wandering onto an unrelated trace. (Verified: on
    // the clean RP2040 corpus boards no pin resolves to a spurious function at
    // this depth - see the calibration in docs/checks/RESOURCE_CONFLICTS.md.)
    if seen.contains(&net_id) || depth > 6 {
        return None;
    }
    // Refuse to traverse a rail/ground net. (The starting pin's own net is a
    // signal net by construction - a GPIO carrying a function - so this only
    // ever blocks a *hop* onto a rail.)
    let net_name = board
        .net(net_id)
        .map(|n| n.name.clone())
        .unwrap_or_default();
    if is_rail_or_ground(&net_name) {
        return None;
    }
    seen.push(net_id);

    // Direct hit: a classifiable target on this very net.
    for (c, _p) in board.net_members(net_id) {
        if is_mcu(c) {
            continue;
        }
        if let Some(target) = classify_target(c) {
            return Some((
                target,
                format!("net '{net_name}' reaches {} ({})", c.reference, c.value),
            ));
        }
    }

    // One hop through a series passive / buffer to the next signal net.
    for (c, _p) in board.net_members(net_id) {
        if is_mcu(c) || !is_series_bridge(c) {
            continue;
        }
        // A multi-pin bridge (a quad line buffer like the 74LVC125, or a FET)
        // has several channels and would otherwise let the walk jump from this
        // signal to an UNRELATED channel's net on the same part. To stay on the
        // intended path, when crossing such a part only continue to a net that
        // carries a *continuing* series passive (the filtered output direction
        // of the PWM-audio reconstruction path), not a bare parallel input. A
        // genuine two-terminal R/L/C bridge has no such ambiguity.
        let multi_channel = c.pins.iter().filter(|p| p.net.is_some()).count() > 2;
        // When the part's gate pairing is known (pin names like 1A/1Y, or the
        // standard quad-buffer pinout), leave ONLY through the output of the
        // gate this net enters. Without it the walk could exit through another
        // channel's output and report the wrong downstream net as evidence.
        let paired = if multi_channel {
            gate_outputs(c, net_id)
        } else {
            None
        };
        for op in &c.pins {
            let Some(oid) = op.net else { continue };
            if oid == net_id {
                continue;
            }
            if paired
                .as_ref()
                .is_some_and(|outs| !outs.contains(&op.number.as_str()))
            {
                continue;
            }
            // Skip a pad that lands on a rail/ground (a bypass cap leg, a buffer
            // supply pin): that is not the signal continuation.
            if let Some(on) = board.net(oid) {
                if is_rail_or_ground(&on.name) {
                    continue;
                }
            }
            if multi_channel
                && paired.is_none()
                && !net_has_series_passive(board, oid, &c.reference)
            {
                continue;
            }
            if let Some((target, chain)) = infer_target(board, is_mcu, oid, depth + 1, seen) {
                return Some((
                    target,
                    format!("net '{net_name}' -> {} -> {chain}", c.reference),
                ));
            }
        }
    }
    None
}

/// The output pin numbers of the gates whose input is on `net_id`, for a
/// multi-channel buffer. `None` when the part's gate pairing is unknown (the
/// caller then falls back to its looser continuation rule); `Some(empty)` when
/// it is known and `net_id` enters no gate input (it is an output or enable
/// net), so the signal does not cross the part from here.
///
/// Pairing comes from the schematic pin names when present (`nA` -> `nY`,
/// `An` -> `Yn`, single-gate `A` -> `Y`). A layout file carries no pin names, so
/// for a 14-pin quad '125 buffer its standard pinout is used (1A=2->1Y=3,
/// 2A=5->2Y=6, 3A=9->3Y=8, 4A=12->4Y=11: TI SN74LVC125A data sheet SCAS290T,
/// Table 4-1 "Pin Functions", p. 3, for the D/DB/NS/PW and BQA/RGY packages).
fn gate_outputs(c: &Component, net_id: i64) -> Option<Vec<&str>> {
    let entry: Vec<&crate::Pin> = c.pins.iter().filter(|p| p.net == Some(net_id)).collect();
    let named = |want: &str| {
        c.pins
            .iter()
            .find(|p| p.function.eq_ignore_ascii_case(want))
            .map(|p| p.number.as_str())
    };
    let gate_no = |n: &&str| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit());
    // The output a gate-input pin name pairs with (1A -> 1Y, A1 -> Y1, A -> Y).
    let paired_name = |f: &str| {
        let f = f.trim().to_ascii_uppercase();
        if f == "A" {
            Some("Y".to_string())
        } else if let Some(n) = f.strip_suffix('A').filter(gate_no) {
            Some(format!("{n}Y"))
        } else {
            f.strip_prefix('A').filter(gate_no).map(|n| format!("Y{n}"))
        }
    };
    let names_known = c.pins.iter().any(|p| {
        paired_name(&p.function)
            .as_deref()
            .and_then(named)
            .is_some()
    });
    if names_known {
        return Some(
            entry
                .iter()
                .filter_map(|p| paired_name(&p.function).as_deref().and_then(named))
                .collect(),
        );
    }
    let v = c.value.to_ascii_lowercase();
    let quad_buffer = v.contains("74")
        && v.contains("125")
        && !v.contains("1g")
        && c.pins
            .iter()
            .all(|p| p.number.parse::<u8>().is_ok_and(|n| (1..=14).contains(&n)))
        && c.pins.iter().all(|p| p.function.is_empty());
    if !quad_buffer {
        return None;
    }
    let mut out = Vec::new();
    for p in &entry {
        let o = match p.number.as_str() {
            "2" => "3",
            "5" => "6",
            "9" => "8",
            "12" => "11",
            _ => continue,
        };
        if let Some(op) = c.pins.iter().find(|q| q.number == o) {
            out.push(op.number.as_str());
        }
    }
    Some(out)
}

/// Does `net_id` carry a two-terminal series passive (R/L/C) other than
/// `exclude_ref`? Used to gate a hop through a multi-channel buffer/FET: the
/// signal continues out of the channel whose net feeds the next filter element,
/// not a net that merely parallels another input.
fn net_has_series_passive(board: &ExtractedBoard, net_id: i64, exclude_ref: &str) -> bool {
    board
        .net_members(net_id)
        .iter()
        .any(|(c, _)| c.reference != exclude_ref && is_two_terminal_passive(c))
}

/// A plain two-terminal R / L / C (the series elements of a filter / divider).
fn is_two_terminal_passive(c: &Component) -> bool {
    RefClass::of(&c.reference).is_rlc() && c.pins.iter().filter(|p| p.net.is_some()).count() == 2
}

/// A part we follow ONE hop through while tracing a signal: a two-terminal
/// series resistor / inductor (TMDS termination, ferrite), a two-terminal
/// capacitor (an AC-coupling cap in the PWM-audio reconstruction path - the
/// walk's rail-skip means a *shunt* cap to ground simply dead-ends, while a
/// *series* coupling cap continues the signal), or a small line buffer / single
/// level-shift FET. Resistor networks / varistors are excluded.
fn is_series_bridge(c: &Component) -> bool {
    let v = c.value.to_ascii_lowercase();
    let lib = c.lib_id.to_ascii_lowercase();
    let connected = c.pins.iter().filter(|p| p.net.is_some()).count();
    // Two-terminal series R / L / C.
    if RefClass::of(&c.reference).is_rlc() {
        return connected == 2;
    }
    // A small logic line buffer (74LVC125 / 74LVC1Gxx) or a single level-shift
    // FET on the signal path. These have a supply pin, so we allow > 2 pins but
    // rely on the rail-skip above to stay on the signal.
    if v.contains("74lvc125")
        || v.contains("74lvc1g")
        || v.contains("bss138")
        || lib.contains("buffer")
    {
        return true;
    }
    false
}

// ---------------------------------------------------------------------------
// The check.
// ---------------------------------------------------------------------------

impl ExtractedBoard {
    /// Run the MCU internal resource-conflict check. Returns its findings in the
    /// same `NetLintReport` shape the connectivity lint uses, so callers and the
    /// CLI can treat them uniformly.
    #[must_use]
    pub fn resource_conflicts(&self) -> NetLintReport {
        let tables = load_resources();
        let mut report = NetLintReport::default();
        check_resource_conflicts(self, &tables, &mut report);
        report
    }
}

fn check_resource_conflicts(
    board: &ExtractedBoard,
    tables: &[McuResources],
    report: &mut NetLintReport,
) {
    for mcu in &board.components {
        let Some(table) = tables.iter().find(|t| t.matches(mcu)) else {
            continue;
        };
        // A fully-routable MCU (ESP32) has no fixed pin->instance conflicts in
        // this class. Stay silent - manufacturing one here would be the exact
        // false positive the calibration discipline forbids.
        if table.fully_routable {
            continue;
        }

        // For each pin of this MCU that carries a resource, infer the function
        // it is used for (if any), keyed by the resource instance it occupies.
        let mut demands: Vec<PinDemand> = Vec::new();
        for pin in &mcu.pins {
            let Some(res) = table.pins.get(&pin.number) else {
                continue;
            };
            let Some(net_id) = pin.net else { continue };
            let net_name = board
                .net(net_id)
                .map(|n| n.name.clone())
                .unwrap_or_default();
            let mut seen = Vec::new();
            let mcu_ref = mcu.reference.clone();
            let is_mcu = move |c: &Component| c.reference == mcu_ref;
            let Some((target, evidence)) = infer_target(board, &is_mcu, net_id, 0, &mut seen)
            else {
                continue;
            };
            let Some(func) = function_for(target, &net_name) else {
                continue;
            };

            // Map the function onto the resource instance the pin occupies, but
            // ONLY when the function's peripheral matches the resource kind.
            // This is the discipline that keeps PIO-driven TMDS data and I2C DDC
            // pins (which reach the display but do not use a PWM slice) from
            // manufacturing false PWM conflicts.
            if let Some(pwm) = &res.pwm {
                if func.peripheral == Peripheral::Pwm {
                    demands.push(PinDemand {
                        pad: pin.number.clone(),
                        label: res.label.clone(),
                        function: func.class.clone(),
                        net: net_name.clone(),
                        evidence: evidence.clone(),
                        resource_kind: "pwm",
                        resource_inst: pwm.clone(),
                    });
                }
            }
            if let Some(group) = &res.qspi_group {
                // The QSPI conflict is ONLY real when a 4-wire SERCOM-SPI signal
                // (MOSI / MISO / SCK / CS by net role) lands on a QSPI DATA pad
                // (PA08..PA11). That is the SparkFun bug: the flash is wired as
                // SERCOM SPI on the QSPI data bus. A flash whose data pads carry
                // quad-IO signals (IO0..IO3 / D0..D3) is a CORRECTLY-wired QSPI
                // flash (e.g. Adafruit Metro/Feather M4) and must NOT fire - the
                // discriminator that keeps the check at zero false positives.
                if func.peripheral == Peripheral::SpiLike
                    && res.qspi_data_pad
                    && net_role_is_4wire_spi(&net_name)
                {
                    demands.push(PinDemand {
                        pad: pin.number.clone(),
                        label: res.label.clone(),
                        function: func.class.clone(),
                        net: net_name.clone(),
                        evidence: evidence.clone(),
                        resource_kind: "qspi",
                        resource_inst: group.clone(),
                    });
                }
            }
        }

        report_pwm_slice_conflicts(mcu, table, &demands, report);
        report_qspi_group_conflicts(mcu, table, &demands, report);
    }
}

/// PWM slice+channel conflict: two pins demanding the SAME {slice,channel} for
/// two DIFFERENT board functions. One channel drives one waveform (the datasheet
/// lets the same channel appear on two GPIOs, but then both carry the SAME
/// signal), so the two functions cannot both use hardware PWM.
///
/// Severity: High ("no valid assignment exists") only when the MCU has no other
/// way to produce the waveform on those pins. When the table declares a
/// software-programmable engine that reaches every GPIO (RP2040 PIO), the pins
/// are fine and the collision binds only firmware that insists on hardware PWM
/// for both functions: a real constraint worth a warning, not a board defect.
fn report_pwm_slice_conflicts(
    mcu: &Component,
    table: &McuResources,
    demands: &[PinDemand],
    report: &mut NetLintReport,
) {
    let mut by_inst: BTreeMap<&str, Vec<&PinDemand>> = BTreeMap::new();
    for d in demands.iter().filter(|d| d.resource_kind == "pwm") {
        by_inst.entry(d.resource_inst.as_str()).or_default().push(d);
    }
    for (inst, ds) in by_inst {
        // distinct functions on the same channel?
        let funcs: std::collections::BTreeSet<&str> =
            ds.iter().map(|d| d.function.as_str()).collect();
        if funcs.len() < 2 {
            continue;
        }
        let chain = ds
            .iter()
            .map(|d| {
                format!(
                    "{} ({} pad {}, net '{}': {})",
                    d.function, d.label, d.pad, d.net, d.evidence
                )
            })
            .collect::<Vec<_>>()
            .join("; and ");
        let head = format!(
            "{} ({}): two functions demand RP2040 PWM slice/channel {inst}, which outputs one waveform (on two pins it is the same signal) [RP2040 datasheet 4.5.2, Table 515, GPIO->slice = (n>>1)&7, ch A/B by parity]: {chain}",
            mcu.reference, mcu.value
        );
        let (severity, message) = match &table.pwm_software_fallback {
            Some(fallback) => (
                Severity::Medium,
                format!(
                    "{head}. Both cannot use hardware PWM at once, so firmware must generate one of them another way: {fallback}. The board wiring itself can work; check that the firmware does this."
                ),
            ),
            None => (Severity::High, head),
        };
        report.findings.push(LintFinding {
            check: LintCheck::McuResourceConflict,
            severity,
            message,
            refs: vec![mcu.reference.clone()],
            nets: ds.iter().map(|d| d.net.clone()).collect(),
        });
    }
}

/// QSPI group conflict: a QSPI-owned pad is used for a non-QSPI function (here,
/// a SPI flash over SERCOM). That commits the shared QSPI pin group to a
/// non-QSPI peripheral, so the QSPI controller cannot use it. High.
fn report_qspi_group_conflicts(
    mcu: &Component,
    table: &McuResources,
    demands: &[PinDemand],
    report: &mut NetLintReport,
) {
    let mut by_group: BTreeMap<&str, Vec<&PinDemand>> = BTreeMap::new();
    for d in demands.iter().filter(|d| d.resource_kind == "qspi") {
        by_group
            .entry(d.resource_inst.as_str())
            .or_default()
            .push(d);
    }
    for (group, ds) in by_group {
        // Require at least two pads of the group committed, so a single stray
        // pad use does not fire - the flash claims four (DATA0..3).
        let pads: std::collections::BTreeSet<&str> = ds.iter().map(|d| d.pad.as_str()).collect();
        if pads.len() < 2 {
            continue;
        }
        let chain = ds
            .iter()
            .map(|d| {
                format!(
                    "{} on {} (pad {}, net '{}': {})",
                    d.function, d.label, d.pad, d.net, d.evidence
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        // Two DIFFERENT functions on one group is a hard conflict: no pin-mux
        // assignment can serve both, so one feature cannot work. That stays
        // serious. ONE self-consistent function occupying the group (a flash
        // wired as plain SERCOM SPI across the QSPI data pads) is NOT a runtime
        // conflict: nothing contends, the board works as shipped, and vendor
        // firmware often drives exactly that arrangement deliberately. The
        // netlist cannot tell deliberate plain-SPI from a botched QSPI intent,
        // so it is a low-severity note saying only what the copper proves, that
        // the QSPI peripheral can never drive these pads.
        let funcs: std::collections::BTreeSet<&str> =
            ds.iter().map(|d| d.function.as_str()).collect();
        let (severity, message) = if funcs.len() >= 2 {
            (
                Severity::High,
                format!(
                    "{} ({}): two different functions occupy {} pads of the fixed QSPI pin group '{group}' (SAM D5x QSPI is pin-locked to PA08..PA11/PB10/PB11, not PORT-routable) [SAM D5x/E5x Data Sheet, Table 6-1 function H, section 36]: {chain}",
                    mcu.reference, mcu.value, pads.len()
                ),
            )
        } else {
            (
                Severity::Low,
                format!(
                    "{} ({}): the fixed QSPI pin group '{group}' is committed to one non-QSPI function across {} pads (SAM D5x QSPI is pin-locked to PA08..PA11/PB10/PB11, not PORT-routable) [SAM D5x/E5x Data Sheet, Table 6-1 function H, section 36]: {chain}. Nothing contends at runtime: firmware that drives this device as plain SERCOM SPI works, and boards shipping this wiring deliberately exist (the SparkFun Thing Plus SAMD51's vendor firmware declares plain SPI flash on exactly these pads). What the netlist does prove is that the QSPI peripheral can never drive these pads, so quad-rate access to this device is off the table; the netlist cannot tell deliberate plain-SPI from an unintended loss of QSPI.",
                    mcu.reference, mcu.value, pads.len()
                ),
            )
        };
        report.findings.push(LintFinding {
            check: LintCheck::McuResourceConflict,
            severity,
            message,
            refs: vec![mcu.reference.clone()],
            nets: ds.iter().map(|d| d.net.clone()).collect(),
        });
        let _ = table;
    }
}

// ---------------------------------------------------------------------------
// Debug introspection (used by examples/resource_probe.rs and tests). Returns,
// for the first matched non-routable MCU, the per-pad (label, net, inferred
// function) so a silent board can be chased to ground truth.
// ---------------------------------------------------------------------------
#[doc(hidden)]
#[must_use]
pub fn debug_demands(board: &ExtractedBoard) -> Vec<(String, String, String, String, String)> {
    let tables = load_resources();
    let mut out = Vec::new();
    for mcu in &board.components {
        let Some(table) = tables.iter().find(|t| t.matches(mcu)) else {
            continue;
        };
        out.push((
            format!("MATCH {}", table.id),
            mcu.reference.clone(),
            mcu.value.clone(),
            mcu.lib_id.clone(),
            format!("fully_routable={}", table.fully_routable),
        ));
        if table.fully_routable {
            continue;
        }
        for pin in &mcu.pins {
            let Some(res) = table.pins.get(&pin.number) else {
                continue;
            };
            let Some(net_id) = pin.net else { continue };
            let net_name = board
                .net(net_id)
                .map(|n| n.name.clone())
                .unwrap_or_default();
            let mut seen = Vec::new();
            let mcu_ref = mcu.reference.clone();
            let is_mcu = move |c: &Component| c.reference == mcu_ref;
            let inf = infer_target(board, &is_mcu, net_id, 0, &mut seen)
                .and_then(|(t, ev)| function_for(t, &net_name).map(|f| (f, ev)));
            out.push((
                res.label.clone(),
                pin.number.clone(),
                net_name,
                res.pwm
                    .clone()
                    .or_else(|| res.qspi_group.clone())
                    .unwrap_or_default(),
                match inf {
                    Some((f, ev)) => format!("{} [{:?}] <= {ev}", f.class, f.peripheral),
                    None => "(no function inferred)".into(),
                },
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;

    #[test]
    fn embedded_table_parses_and_has_known_mcus() {
        let t = load_resources();
        assert!(t.iter().any(|m| m.id == "rp2040_pico_module"));
        assert!(t.iter().any(|m| m.id == "samd51j_tqfp64"));
        assert!(t
            .iter()
            .any(|m| m.id == "esp32_gpio_matrix" && m.fully_routable));
    }

    #[test]
    fn rp2040_pwm_rule_gp12_and_gp28_are_slice_6a() {
        let t = load_resources();
        let m = t.iter().find(|m| m.id == "rp2040_pico_module").unwrap();
        // pad 16 = GP12, pad 34 = GP28: the bug's two pins, both PWM 6A.
        assert_eq!(m.pins["16"].pwm.as_deref(), Some("6A"));
        assert_eq!(m.pins["16"].label, "GP12");
        assert_eq!(m.pins["34"].pwm.as_deref(), Some("6A"));
        assert_eq!(m.pins["34"].label, "GP28");
    }

    #[test]
    fn rp2040_qfn_gpio_resources_match_the_sourced_package_model() {
        let tables = load_resources();
        let resources = tables.iter().find(|m| m.id == "rp2040_qfn56").unwrap();
        let model = hauksbee_models::ModelLibrary::builtin()
            .resolve(&hauksbee_models::ComponentQuery {
                value: Some("RP2040".into()),
                footprint: Some("QFN-56".into()),
                ..Default::default()
            })
            .model
            .expect("known RP2040 package");
        assert_eq!(resources.pins.len(), 30);
        assert!(
            !resources.pins.contains_key("33"),
            "pad33 is IOVDD, not GPIO22"
        );
        for (pad, pin) in &resources.pins {
            let gpio: u8 = pin.label.strip_prefix("GP").unwrap().parse().unwrap();
            let role = model.pins[pad].split('_').next().unwrap();
            assert_eq!(role, format!("gpio{gpio}"), "pad {pad}");
            let pwm = format!(
                "{}{}",
                (gpio >> 1) & 7,
                if gpio.is_multiple_of(2) { 'A' } else { 'B' }
            );
            assert_eq!(pin.pwm.as_deref(), Some(pwm.as_str()), "pad {pad}");
        }
    }

    #[test]
    fn samd51_qspi_group_covers_pa08_pa11() {
        let t = load_resources();
        let m = t.iter().find(|m| m.id == "samd51j_tqfp64").unwrap();
        for (pad, port) in [
            ("17", "PA08"),
            ("18", "PA09"),
            ("19", "PA10"),
            ("20", "PA11"),
        ] {
            assert_eq!(m.pins[pad].label, port);
            assert_eq!(m.pins[pad].qspi_group.as_deref(), Some("qspi_data"));
        }
    }

    #[test]
    fn match_patterns_select_the_right_mcu() {
        let t = load_resources();
        let by_value = |v: &str| {
            t.iter()
                .find(|m| {
                    m.value_re.is_match(v) || m.lib_re.as_ref().is_some_and(|r| r.is_match(v))
                })
                .map(|m| m.id.as_str())
        };
        assert_eq!(by_value("RP2040_PLATFORM"), Some("rp2040_pico_module"));
        assert_eq!(by_value("ATSAMD51J20A-A"), Some("samd51j_tqfp64"));
        assert_eq!(by_value("ESP32-WROOM-32"), Some("esp32_gpio_matrix"));
        // The bare RP2040 must hit the QFN table, not the module table.
        let bare = t
            .iter()
            .find(|m| m.value_re.is_match("RP2040") && m.id == "rp2040_qfn56");
        assert!(bare.is_some());
    }

    // -- Offline synthetic boards (corpus-free; always run) -------------------
    //
    // Build a tiny KiCad netlist in memory and drive the real check, so the
    // core logic is guarded on any runner. The shape mirrors the corpus boards:
    // a Pico-form module, an HDMI connector on the DVI clock pad, an audio jack
    // reached through a series resistor.

    fn msgs(text: &str) -> Vec<String> {
        let board = ExtractedBoard::from_kicad_netlist(text).expect("synthetic netlist parses");
        board
            .resource_conflicts()
            .findings
            .iter()
            .map(|f| f.message.clone())
            .collect()
    }

    /// GP12 (pad 16, slice 6A) -> HDMI clock, GP28 (pad 34, slice 6A) -> audio
    /// jack via a series R. The two PWM functions collide on slice 6A: must fire.
    /// U1 carries >=30 connected pins (most on a shared spare net) to satisfy the
    /// module's `min_pins` guard. Built by `conflict_net()` so the filler is not a
    /// hand-balanced wall of parens.
    fn conflict_net() -> String {
        // Spare nodes to pad U1's connected-pin count over the min_pins=30 guard.
        let mut spare = String::new();
        for pin in [
            1, 2, 4, 5, 6, 7, 9, 10, 11, 12, 14, 15, 17, 18, 19, 20, 21, 22, 24, 25, 26, 27, 29,
            31, 3, 8, 13, 23, 28, 30, 32, 35, 36, 37, 38, 39, 40,
        ] {
            let _ = writeln!(
                spare,
                "    (net (code {}) (name spare{pin}) (node (ref U1) (pin {pin})) (node (ref RES1) (pin 1)))",
                100 + pin
            );
        }
        format!(
            r"(export (version D)
  (components
    (comp (ref U1) (value RP2040_PLATFORM) (libsource (lib OLIMEX_Cases) (part RP2040_PLATFORM)))
    (comp (ref HDMI1) (value HDMI-SWM-19) (libsource (lib conn) (part HDMI-SWM-19)))
    (comp (ref AUDIO1) (value PJ-W47S) (libsource (lib conn) (part AUDIO_JACK)))
    (comp (ref R1) (value 270) (libsource (lib dev) (part R)))
    (comp (ref RES1) (value pad) (libsource (lib x) (part p))))
  (nets
    (net (code 1) (name /CK) (node (ref U1) (pin 16)) (node (ref HDMI1) (pin 12)))
    (net (code 2) (name /PWM_L) (node (ref U1) (pin 34)) (node (ref R1) (pin 1)))
    (net (code 3) (name /PWM_AUDIO_L) (node (ref R1) (pin 2)) (node (ref AUDIO1) (pin 2)))
{spare}  ))"
        )
    }

    #[test]
    fn synthetic_pwm_slice_6a_conflict_fires() {
        let m = msgs(&conflict_net());
        assert_eq!(m.len(), 1, "expected one slice-6A conflict, got {m:#?}");
        assert!(m[0].contains("6A"), "{}", m[0]);
        assert!(m[0].contains("GP12") && m[0].contains("GP28"), "{}", m[0]);
    }

    #[test]
    fn synthetic_dvi_clock_moved_off_slice_6_is_clean() {
        // Move the DVI clock from GP12 (pad 16, 6A) to GP14 (pad 19, 7A): now the
        // audio on 6A has no PWM twin. Must be silent (the rev-B-NETLIST
        // discriminator: that file, a rev-A schematic export, routes the clock
        // on slice 7; the rev B layout and Gerbers carry the slice-6 conflict).
        // Pad 19 is otherwise a spare node here, so dropping it from the spare
        // net and giving it the clock keeps the pin count valid.
        let moved = conflict_net()
            .replace(
                "(node (ref U1) (pin 19)) (node (ref RES1) (pin 1))",
                "(node (ref RES1) (pin 1))",
            )
            .replace("(node (ref U1) (pin 16))", "(node (ref U1) (pin 19))");
        assert!(
            msgs(&moved).is_empty(),
            "clock on slice 7 must not collide with audio on 6A"
        );
    }

    #[test]
    fn synthetic_audio_only_no_dvi_is_clean() {
        // Audio on slice 6A but NO DVI clock anywhere: one demand, no conflict.
        let no_dvi = conflict_net().replace(
            "(net (code 1) (name /CK) (node (ref U1) (pin 16)) (node (ref HDMI1) (pin 12)))",
            "",
        );
        assert!(
            msgs(&no_dvi).is_empty(),
            "a single PWM demand must not fire"
        );
    }

    fn findings_with(text: &str, tables: &[McuResources]) -> Vec<LintFinding> {
        let board = ExtractedBoard::from_kicad_netlist(text).expect("synthetic netlist parses");
        let mut report = NetLintReport::default();
        check_resource_conflicts(&board, tables, &mut report);
        report.findings
    }

    #[test]
    fn pwm_collision_with_pio_fallback_is_a_warning_naming_the_fallback() {
        // RP2040: PIO reaches every GPIO, so the slice-6A collision is a firmware
        // constraint. Still reported, but as a warning that names the remedy.
        let f = findings_with(&conflict_net(), &load_resources());
        assert_eq!(f.len(), 1, "{f:#?}");
        assert_eq!(f[0].severity, Severity::Medium);
        assert!(
            f[0].message.contains("6A") && f[0].message.contains("PIO state machine"),
            "{}",
            f[0].message
        );
    }

    #[test]
    fn pwm_collision_without_software_fallback_stays_serious() {
        // The same wiring on an MCU whose table declares no engine able to drive
        // the pins another way: no valid assignment exists, so it is High.
        let mut tables = load_resources();
        for t in &mut tables {
            t.pwm_software_fallback = None;
        }
        let f = findings_with(&conflict_net(), &tables);
        assert_eq!(f.len(), 1, "{f:#?}");
        assert_eq!(f[0].severity, Severity::High);
        assert!(
            !f[0].message.contains("PIO state machine"),
            "{}",
            f[0].message
        );
    }

    /// A conflict board whose audio path crosses a quad '125 buffer: GP28 enters
    /// gate 1 (pin 2 -> pin 3, then R1 -> left jack contact), while gate 4 (pin
    /// 12 -> pin 11, then R2 -> right contact) carries an unrelated signal.
    /// `named` adds schematic pin names (1A/1Y...); without them the walk must
    /// fall back to the part's standard pinout.
    fn buffered_audio_net(named: bool) -> String {
        let pf = |n: &str| {
            if named {
                format!(" (pinfunction {n})")
            } else {
                String::new()
            }
        };
        conflict_net()
            .replace(
                "(comp (ref R1) (value 270) (libsource (lib dev) (part R)))",
                "(comp (ref R1) (value 270) (libsource (lib dev) (part R)))\n    (comp (ref R2) (value 270) (libsource (lib dev) (part R)))\n    (comp (ref U3) (value 74LVC125APW) (libsource (lib ic) (part 74LVC125)))",
            )
            .replace(
                "(net (code 2) (name /PWM_L) (node (ref U1) (pin 34)) (node (ref R1) (pin 1)))",
                &format!(
                    "(net (code 2) (name /PWM_L) (node (ref U1) (pin 34)) (node (ref U3) (pin 2){}))\n    (net (code 4) (name /BUF_R) (node (ref U3) (pin 11){}) (node (ref R2) (pin 1)))\n    (net (code 5) (name /BUF_L) (node (ref U3) (pin 3){}) (node (ref R1) (pin 1)))\n    (net (code 6) (name /AUDIO_R) (node (ref R2) (pin 2)) (node (ref AUDIO1) (pin 3)))\n    (net (code 7) (name /OTHER) (node (ref U3) (pin 12){}))",
                    pf("1A"),
                    pf("4Y"),
                    pf("1Y"),
                    pf("4A")
                ),
            )
    }

    #[test]
    fn buffer_crossing_follows_the_entered_gate_not_another_channel() {
        for named in [true, false] {
            let f = findings_with(&buffered_audio_net(named), &load_resources());
            assert_eq!(f.len(), 1, "named={named}: {f:#?}");
            let m = &f[0].message;
            assert!(
                m.contains("'/BUF_L'") && m.contains("/PWM_AUDIO_L"),
                "named={named}: evidence must follow gate 1 to the left channel: {m}"
            );
            assert!(
                !m.contains("/BUF_R") && !m.contains("/AUDIO_R"),
                "named={named}: evidence jumped to another gate's output: {m}"
            );
        }
    }

    #[test]
    fn gate_pairing_exits_only_the_entered_gate_and_never_from_an_output() {
        // A quad buffer, pins 1..14, each on its own net (net id = pin number).
        let quad = |named: bool| Component {
            reference: "U9".into(),
            value: "74HC125".into(),
            pins: (1..=14)
                .map(|n| crate::Pin {
                    number: n.to_string(),
                    net: Some(n),
                    function: if named {
                        [
                            "1OE", "1A", "1Y", "2OE", "2A", "2Y", "GND", "3Y", "3A", "3OE", "4Y",
                            "4A", "4OE", "VCC",
                        ][usize::try_from(n - 1).unwrap()]
                        .into()
                    } else {
                        String::new()
                    },
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        for named in [true, false] {
            let c = quad(named);
            assert_eq!(
                gate_outputs(&c, 9),
                Some(vec!["8"]),
                "named={named}: 3A -> 3Y"
            );
            assert_eq!(
                gate_outputs(&c, 12),
                Some(vec!["11"]),
                "named={named}: 4A -> 4Y"
            );
            // Entering on an output (or enable) pin: known part, no crossing.
            assert_eq!(
                gate_outputs(&c, 6),
                Some(vec![]),
                "named={named}: 2Y is an output"
            );
            assert_eq!(
                gate_outputs(&c, 1),
                Some(vec![]),
                "named={named}: 1OE is an enable"
            );
        }
        // An unknown multi-pin part: pairing unknown, caller keeps its fallback.
        let mut other = quad(false);
        other.value = "SN65LVDS".into();
        assert_eq!(gate_outputs(&other, 2), None);
    }

    #[test]
    fn rail_or_ground_is_token_matched_not_substring() {
        // True rails / grounds.
        for n in [
            "GND",
            "+3V3",
            "/Power/+5V",
            "VBUS",
            "3V3",
            "1V8",
            "AGND",
            "VDDA",
            "GNDPWR",
        ] {
            assert!(is_rail_or_ground(n), "{n} should be a rail/ground");
        }
        // Signal nets that merely EMBED a *voltage code* must NOT be treated as
        // rails: with a non-supply word present (EN/OK/PG), the voltage code is
        // part of a signal name, not the rail itself. (A bare VCC/VDD/GND token
        // stays classed as a rail even in a compound - the safe direction, since
        // misclassifying a rail-ish net only ever yields a false negative.)
        for n in [
            "5V_EN",
            "PWR_3V3_OK",
            "FLASH_MISO",
            "/PWM_L",
            "PG_5V",
            "CK_5V0_OK",
        ] {
            assert!(!is_rail_or_ground(n), "{n} is a signal, not a rail");
        }
    }

    #[test]
    fn net_role_4wire_spi_discriminates_from_quad_io() {
        // 4-wire SPI roles (the bug's naming on the QSPI data pads): fire.
        for n in [
            "FLASH_MOSI",
            "FLASH_MISO",
            "/FLASH_SCK",
            "FLASH_CS",
            "SDI",
            "COPI",
        ] {
            assert!(net_role_is_4wire_spi(n), "{n} should be a 4-wire SPI role");
        }
        // Quad-IO data lanes (a correct QSPI flash): do NOT fire.
        for n in [
            "FLASH_IO0",
            "FLASH_IO3",
            "QSPI_D2",
            "FLASH_DAT1",
            "FLASH_SD0",
        ] {
            assert!(
                !net_role_is_4wire_spi(n),
                "{n} is quad-IO, not a 4-wire SPI role"
            );
        }
        // Unrelated nets: do not fire.
        assert!(!net_role_is_4wire_spi("/PWM_L"));
        assert!(!net_role_is_4wire_spi("GND"));
    }

    /// A SAMD51 with a flash on the QSPI DATA pads. With 4-wire SPI net naming
    /// (MOSI/MISO/SCK/CS) it is the SparkFun SERCOM-SPI bug -> fires. With
    /// quad-IO naming (IO0..IO3) it is a correct QSPI flash -> silent.
    #[allow(clippy::format_collect, reason = "forty-line test fixture")]
    fn samd51_flash_net(data_names: [&str; 4]) -> String {
        format!(
            r"(export (version D)
  (components
    (comp (ref U1) (value ATSAMD51J20A-A) (libsource (lib mcu) (part ATSAMD51J20A)))
    (comp (ref U2) (value SPI Flash) (libsource (lib Memory) (part AT25SF041)))
    (comp (ref FILL) (value pad) (libsource (lib x) (part p))))
  (nets
    (net (code 1) (name {d0}) (node (ref U1) (pin 17)) (node (ref U2) (pin 5)))
    (net (code 2) (name {d1}) (node (ref U1) (pin 18)) (node (ref U2) (pin 6)))
    (net (code 3) (name {d2}) (node (ref U1) (pin 19)) (node (ref U2) (pin 1)))
    (net (code 4) (name {d3}) (node (ref U1) (pin 20)) (node (ref U2) (pin 2)))
{fill}  ))",
            d0 = data_names[0], d1 = data_names[1], d2 = data_names[2], d3 = data_names[3],
            fill = (5..=44)
                .map(|p| format!("    (net (code {}) (name f{p}) (node (ref U1) (pin {p})) (node (ref FILL) (pin 1)))\n", 100 + p))
                .collect::<String>(),
        )
    }

    #[test]
    fn synthetic_samd51_sercom_spi_flash_on_qspi_pads_is_low_severity_fact() {
        // One self-consistent function (a flash as plain SERCOM SPI) occupying
        // the QSPI data group is a true, useful fact ("QSPI can never drive
        // this") but NOT a runtime conflict: boards ship this wiring on purpose
        // (SparkFun Thing Plus SAMD51, vendor-firmware confirmed). It must
        // surface as Low, never High - High here was the campaign's confirmed
        // severity overclaim.
        let board = ExtractedBoard::from_kicad_netlist(&samd51_flash_net([
            "FLASH_MOSI",
            "FLASH_SCK",
            "FLASH_CS",
            "FLASH_MISO",
        ]))
        .expect("synthetic netlist parses");
        let findings = board.resource_conflicts().findings;
        assert_eq!(
            findings.len(),
            1,
            "the single-function QSPI-group note must fire once, got {findings:#?}"
        );
        assert_eq!(
            findings[0].severity,
            Severity::Low,
            "single-function group occupation must not be serious: {}",
            findings[0].message
        );
        assert!(
            findings[0].message.contains("qspi_data")
                && findings[0].message.contains("SPI flash")
                && findings[0].message.contains("can never drive"),
            "{}",
            findings[0].message
        );
    }

    #[test]
    fn synthetic_samd51_correct_qspi_flash_is_silent() {
        // Same flash, same pads, but quad-IO naming: a correct QSPI flash.
        let m = msgs(&samd51_flash_net([
            "FLASH_IO0",
            "FLASH_IO1",
            "FLASH_IO2",
            "FLASH_IO3",
        ]));
        assert!(
            m.is_empty(),
            "a correctly-wired QSPI flash must NOT fire, got {m:#?}"
        );
    }
}
