//! Digital-audio bus wiring (I2S, left-justified, TDM) read from pad roles.
//!
//! An audio bus has a fixed anatomy: a bit clock (BCLK), a frame clock (LRCLK or
//! WS), data lines, and for many codecs a master clock (MCLK). Exactly one part
//! drives each clock and every other part listens; each data line has its
//! transmitters and receivers. A wiring slip does not look wrong on a schematic
//! (every pad is wired to something) and does not fail a power or level check,
//! but the bus is dead or noisy: two transmitters on one data line, a data line
//! nothing transmits on, BCLK on the LRCLK pad, a clock nobody (or two parts)
//! drive, a codec that needs MCLK with none connected.
//!
//! ## What the check reads
//!
//! * The pad roles of a part's card ([`hauksbee_models::audio_roles`]): a part
//!   is read as audio only when its card names both a bit clock and a frame
//!   clock, and a multiplexed pad (`pb5_sck`) is never an audio pad.
//! * `i2s_clock_role` on the card (`master`, `slave`; anything else, or
//!   absent, means the part can be either): who drives the part's BCLK and
//!   LRCLK pads. A clock finding that needs a direction fires only on
//!   directions cards state.
//! * `mclk_required` (true when the datasheet says the part cannot run
//!   without a master clock) and `dout_hiz` (true when the data output
//!   tri-states outside its slot, the TDM idiom) on the card.
//!
//! * A net whose NAME plainly states one bus signal (`I2S_BCLK`, `MCLK`,
//!   `SAI1_FS_A`; see [`hauksbee_models::audio_roles::net_signal`]) landing on
//!   a pad whose card role is a different bus signal is swapped wiring, said
//!   even when the net also holds pins the check cannot classify. A data net
//!   against a data pad is not compared (a net name does not say which end
//!   transmits).
//!
//! ## What keeps it silent
//!
//! The check fires on a net only when every non-passive pin on it has a known
//! audio role. A pin it cannot classify (an MCU pad that is a GPIO until muxed,
//! a connector, a buffer) leaves the net unjudged, as does a series resistor,
//! ferrite or coupling capacitor that continues the line onto another net.
//! Legal arrangements stay quiet: master or slave in either direction, a shared
//! TDM data line (outputs that tri-state, three or more outputs, or a net named
//! for TDM), and a codec whose card does not say it needs MCLK.

use std::collections::{BTreeSet, HashMap};

use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    Component, ExtractedBoard, LintCheck, LintFinding, NetLintReport, Severity, REMEDY_MARKER,
};
use hauksbee_models::audio_roles::{audio_pads, net_signal, AudioRole};
use hauksbee_models::{ModelEntry, ModelLibrary};

use hauksbee_bind::binder::power_rail_voltage;

/// Who drives a part's clock pads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClockDir {
    /// The part drives BCLK and LRCLK (its card says `master`).
    Drives,
    /// The part listens (its card says `slave`).
    Listens,
    /// The card does not say, or says the part is configurable.
    Unknown,
}

/// A part the audio vocabulary applies to.
struct AudioPart {
    model: ModelEntry,
    pads: std::collections::BTreeMap<String, AudioRole>,
    dir: ClockDir,
}

/// One audio pin on a net.
struct Member<'a> {
    comp: &'a Component,
    pad: &'a str,
    role: AudioRole,
    dir: ClockDir,
}

/// The audio parts of the board, by reference.
fn audio_parts(board: &ExtractedBoard, lib: &ModelLibrary) -> HashMap<String, AudioPart> {
    let mut parts = HashMap::new();
    for (comp, model) in super::resolved_components(board, lib) {
        let Some(pads) = audio_pads(&model.pins) else {
            continue;
        };
        let dir = match model
            .params
            .get_str("i2s_clock_role")
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("master") => ClockDir::Drives,
            Some("slave") => ClockDir::Listens,
            _ => ClockDir::Unknown,
        };
        parts.insert(comp.reference.clone(), AudioPart { model, pads, dir });
    }
    parts
}

/// A passive that cannot drive or receive audio by itself.
fn is_passive(c: &Component) -> bool {
    hauksbee_extract::is_resistor(c)
        || hauksbee_extract::is_capacitor(c)
        || hauksbee_extract::is_plain_inductor(c)
        || hauksbee_extract::is_test_point_or_hole(c)
}

/// Whether a passive on the net leads the line on to another signal net (a
/// series damping resistor, a ferrite, a coupling capacitor): the net is then
/// one segment of a longer line this check does not follow.
fn continues_elsewhere(board: &ExtractedBoard, c: &Component, net_id: i64) -> bool {
    if hauksbee_extract::is_test_point_or_hole(c) {
        return false;
    }
    c.pins
        .iter()
        .filter_map(|p| p.net)
        .filter(|&id| id != 0 && id != net_id)
        .filter_map(|id| board.net(id))
        .any(|n| !super::is_ground_name(&n.name) && power_rail_voltage(&n.name).is_none())
}

/// The audio pins on `net_id` when every non-passive pin is classified and no
/// passive continues the line; `None` when the net cannot be judged.
fn members<'a>(
    board: &'a ExtractedBoard,
    parts: &'a HashMap<String, AudioPart>,
    net_id: i64,
) -> Option<Vec<Member<'a>>> {
    let mut out: Vec<Member<'a>> = Vec::new();
    for (c, p) in board.net_members(net_id) {
        if !AssemblyState::of(c).is_present() {
            continue;
        }
        if is_passive(c) {
            if continues_elsewhere(board, c, net_id) {
                return None;
            }
            continue;
        }
        let part = parts.get(&c.reference)?;
        let role = *part.pads.get(&p.number)?;
        if out
            .iter()
            .any(|m| m.comp.reference == c.reference && m.pad == p.number)
        {
            continue; // a both-sided record lists a pad twice
        }
        out.push(Member {
            comp: c,
            pad: &p.number,
            role,
            dir: part.dir,
        });
    }
    (!out.is_empty()).then_some(out)
}

/// The members of `ms` that satisfy `keep`.
fn select<'a, 'b>(ms: &'a [Member<'b>], keep: impl Fn(&Member<'b>) -> bool) -> Vec<&'a Member<'b>> {
    ms.iter().filter(|m| keep(m)).collect()
}

fn names(ms: &[&Member<'_>]) -> String {
    ms.iter()
        .map(|m| format!("{} pad {}", m.comp.reference, m.pad))
        .collect::<Vec<_>>()
        .join(" and ")
}

fn refs_of(ms: &[Member<'_>]) -> Vec<String> {
    let mut refs: Vec<String> = Vec::new();
    for m in ms {
        if !refs.contains(&m.comp.reference) {
            refs.push(m.comp.reference.clone());
        }
    }
    refs
}

fn finding(severity: Severity, message: String, refs: Vec<String>, net: &str) -> LintFinding {
    LintFinding {
        check: LintCheck::AudioBusWiring,
        severity,
        message,
        refs,
        nets: vec![net.to_string()],
    }
}

/// Whether the data outputs on a line time-share it (TDM): the line is named
/// for it, three or more parts transmit on it, or every transmitter's card
/// says its output tri-states.
fn is_tdm(net: &str, outs: &[&Member<'_>], parts: &HashMap<String, AudioPart>) -> bool {
    net.to_ascii_lowercase().contains("tdm")
        || outs.len() >= 3
        || outs.iter().all(|m| {
            parts
                .get(&m.comp.reference)
                .and_then(|p| p.model.params.get_bool("dout_hiz"))
                == Some(true)
        })
}

/// The finding for one net's audio pins, if they are wired against their roles.
fn judge(net: &str, ms: &[Member<'_>], parts: &HashMap<String, AudioPart>) -> Option<LintFinding> {
    let refs = refs_of(ms);
    let class_of = |class: AudioRole| {
        select(ms, |m| {
            if class == AudioRole::Data {
                m.role.is_data()
            } else {
                m.role == class
            }
        })
    };

    // A clock on a data pad, or one clock on another's: the roles disagree.
    let classes: Vec<AudioRole> = [
        AudioRole::BitClock,
        AudioRole::FrameClock,
        AudioRole::MasterClock,
        AudioRole::Data,
    ]
    .into_iter()
    .filter(|class| !class_of(*class).is_empty())
    .collect();
    if let [first, second, ..] = classes.as_slice() {
        return Some(finding(
            Severity::High,
            format!(
                "'{net}' joins the {} of {} to the {} of {}: the audio roles disagree, so the \
                 bus cannot run. {REMEDY_MARKER}Move each pin to the net that carries its own \
                 signal (check the codec's pin table against the controller's).",
                first.label(),
                names(&class_of(*first)),
                second.label(),
                names(&class_of(*second)),
            ),
            refs,
            net,
        ));
    }

    judge_clocks(net, ms, &refs).or_else(|| judge_data(net, ms, parts, &refs))
}

/// Who drives each clock: two drivers, or none.
fn judge_clocks(net: &str, ms: &[Member<'_>], refs: &[String]) -> Option<LintFinding> {
    let of = |role: AudioRole| select(ms, |m| m.role == role);
    for clock in [AudioRole::BitClock, AudioRole::FrameClock] {
        let on = of(clock);
        if on.is_empty() {
            continue;
        }
        let drivers: Vec<&Member<'_>> = on
            .iter()
            .copied()
            .filter(|m| m.dir == ClockDir::Drives)
            .collect();
        if drivers.len() >= 2 {
            return Some(finding(
                Severity::High,
                format!(
                    "'{net}' carries the {} and {} both drive it (each card says it is the bus \
                     master): two drivers fight on one clock. {REMEDY_MARKER}Make one of the \
                     two the slave, or give each master its own bus.",
                    clock.label(),
                    names(&drivers),
                ),
                refs.to_vec(),
                net,
            ));
        }
        if on.iter().all(|m| m.dir == ClockDir::Listens) {
            return Some(finding(
                Severity::Medium,
                format!(
                    "'{net}' carries the {} and nothing drives it: {} {} all slaves (their \
                     cards say they only listen), so the bus never clocks. {REMEDY_MARKER}Make \
                     one part the master of this bus, or wire in the clock source.",
                    clock.label(),
                    names(&on),
                    if on.len() == 1 { "is a" } else { "are" },
                ),
                refs.to_vec(),
                net,
            ));
        }
    }

    None
}

/// The data line: transmitters and receivers.
fn judge_data(
    net: &str,
    ms: &[Member<'_>],
    parts: &HashMap<String, AudioPart>,
    refs: &[String],
) -> Option<LintFinding> {
    let of = |role: AudioRole| select(ms, |m| m.role == role);
    let outs = of(AudioRole::DataOut);
    let ins = of(AudioRole::DataIn);
    let bidir = of(AudioRole::Data);
    if outs.len() >= 2 && !is_tdm(net, &outs, parts) {
        return Some(finding(
            Severity::Medium,
            format!(
                "'{net}' joins the data outputs of {}: two transmitters drive one data line, so \
                 the bits corrupt wherever they disagree (unless both tri-state outside their \
                 own time slot, as in TDM, which neither card says). {REMEDY_MARKER}Give each \
                 output its own receiver's data input, or set the cards' dout_hiz when the \
                 parts time-share the line.",
                names(&outs),
            ),
            refs.to_vec(),
            net,
        ));
    }
    if outs.is_empty() && bidir.is_empty() && ins.len() >= 2 {
        return Some(finding(
            Severity::Medium,
            format!(
                "'{net}' joins the data inputs of {} and nothing transmits on it: the line has \
                 no data output, so both receivers read a floating level. {REMEDY_MARKER}Wire \
                 the line to the data output of the part that sends the audio.",
                names(&ins),
            ),
            refs.to_vec(),
            net,
        ));
    }
    None
}

/// The audio-bus class of a role, for comparing a net's stated signal with a
/// pad's: all data directions are one class.
const fn class_of_role(role: AudioRole) -> AudioRole {
    if role.is_data() {
        AudioRole::Data
    } else {
        role
    }
}

/// A net named for one bus signal with a pad of a different bus signal on it.
fn swapped_by_name(
    board: &ExtractedBoard,
    parts: &HashMap<String, AudioPart>,
    net: &hauksbee_extract::Net,
) -> Option<LintFinding> {
    let stated = class_of_role(net_signal(&net.name)?);
    let mut wrong: Vec<(&Component, &str, AudioRole)> = Vec::new();
    for (c, p) in board.net_members(net.id) {
        if !AssemblyState::of(c).is_present() || is_passive(c) {
            continue;
        }
        let Some(role) = parts.get(&c.reference).and_then(|x| x.pads.get(&p.number)) else {
            continue;
        };
        let already = wrong
            .iter()
            .any(|(w, pad, _)| w.reference == c.reference && *pad == p.number);
        if class_of_role(*role) != stated && !already {
            wrong.push((c, &p.number, *role));
        }
    }
    let first = wrong.first()?;
    let pads: Vec<String> = wrong
        .iter()
        .map(|(c, pad, role)| {
            format!(
                "{} pad {pad} (the {} on its card)",
                c.reference,
                role.label()
            )
        })
        .collect();
    let mut refs: Vec<String> = Vec::new();
    for (c, _, _) in &wrong {
        if !refs.contains(&c.reference) {
            refs.push(c.reference.clone());
        }
    }
    Some(finding(
        Severity::Medium,
        format!(
            "'{}' is named for the {} but {} sits on it: the wiring looks swapped, so the bus \
             cannot run unless the net name is the one that is wrong. {REMEDY_MARKER}Move {} to \
             the net that carries its own signal (check the codec's pin table against the \
             controller's), or rename the net if the pad really is wired as intended.",
            net.name,
            stated.label(),
            pads.join(" and "),
            first.0.reference,
        ),
        refs,
        &net.name,
    ))
}

/// A codec that needs MCLK with its master-clock pad connected to nothing.
fn missing_mclk(
    board: &ExtractedBoard,
    parts: &HashMap<String, AudioPart>,
    comp: &Component,
) -> Vec<LintFinding> {
    let Some(part) = parts.get(&comp.reference) else {
        return Vec::new();
    };
    if part.model.params.get_bool("mclk_required") != Some(true) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (pad, _) in part
        .pads
        .iter()
        .filter(|(_, role)| **role == AudioRole::MasterClock)
    {
        let Some(pin) = comp.pins.iter().find(|p| &p.number == pad) else {
            continue;
        };
        let net = pin.net.filter(|&id| id != 0).and_then(|id| board.net(id));
        let driven = net.is_some_and(|n| {
            !super::is_unconnected_net(&n.name)
                && board.net_members(n.id).iter().any(|(c, p)| {
                    AssemblyState::of(c).is_present()
                        && !is_passive(c)
                        && !(c.reference == comp.reference && p.number == *pad)
                })
        });
        if !driven {
            out.push(finding(
                Severity::High,
                format!(
                    "{} pad {pad} is the master clock (MCLK) and its card says the part cannot \
                     run without one, but nothing drives the pad. {REMEDY_MARKER}Connect MCLK \
                     to the controller's master-clock output or a clock oscillator.",
                    comp.reference,
                ),
                vec![comp.reference.clone()],
                net.map_or("", |n| n.name.as_str()),
            ));
        }
    }
    out
}

/// Run the audio-bus lint over every net with an audio pin.
#[must_use]
pub fn audio_bus_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = NetLintReport::default();
    let parts = audio_parts(board, lib);
    if parts.is_empty() {
        return report;
    }
    let mut seen: BTreeSet<i64> = BTreeSet::new();
    for net in &board.nets {
        if net.id == 0
            || !seen.insert(net.id)
            || power_rail_voltage(&net.name).is_some()
            || super::is_ground_name(&net.name)
        {
            continue;
        }
        let judged = members(board, &parts, net.id).and_then(|ms| judge(&net.name, &ms, &parts));
        // The roles-disagree finding already says it; the name is the fallback
        // for a net the roles cannot judge.
        report
            .findings
            .extend(judged.or_else(|| swapped_by_name(board, &parts, net)));
    }
    for comp in &board.components {
        if AssemblyState::of(comp).is_present() {
            report.findings.extend(missing_mclk(board, &parts, comp));
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{board, part};
    use super::*;
    use hauksbee_extract::Pin;
    use std::fmt::Write as _;

    /// A user card file for an audio part: `value` matches, with these pins
    /// and params.
    fn card(id: &str, value: &str, params: &str, pins: &[(&str, &str)]) -> String {
        let pins = pins.iter().fold(String::new(), |mut out, (pad, role)| {
            let _ = writeln!(out, "\"{pad}\" = \"{role}\"");
            out
        });
        format!(
            r#"
[[models]]
id = "{id}"
kind = "digital"
description = "audio fixture"
[models.match]
value_re = "^{value}$"
[models.params]
identity_only = true
warning = "identity only"
unlocked_by = "validated behavior"
{params}
[models.pins]
{pins}"#
        )
    }

    /// The pad map every fixture uses: 1 BCLK, 2 LRCLK, 3 DIN, 4 DOUT, 5 MCLK,
    /// 6 VDD, 7 GND.
    const PADS: &[(&str, &str)] = &[
        ("1", "bclk"),
        ("2", "lrclk"),
        ("3", "sdin"),
        ("4", "sdout"),
        ("5", "mclk"),
        ("6", "vdd"),
        ("7", "gnd"),
    ];

    fn library(cards: &[String], tag: &str) -> ModelLibrary {
        let dir =
            std::env::temp_dir().join(format!("hauksbee_audio_bus_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("models.toml"), cards.concat()).unwrap();
        let mut lib = ModelLibrary::empty();
        assert!(lib.load_user_dir(&dir).is_empty(), "fixture cards load");
        lib
    }

    /// The fixture cards: a controller (`CTL`) and codecs (`CODEC`, `ADC`),
    /// each with the role and requirements `params` states.
    fn cards(ctl: &str, codec: &str, adc: &str, tag: &str) -> ModelLibrary {
        library(
            &[
                card("ctl", "CTL", ctl, PADS),
                card("codec", "CODEC", codec, PADS),
                card("adc", "ADC", adc, PADS),
            ],
            tag,
        )
    }

    /// Nets: 1 GND, 2 +3V3, 10 BCLK, 11 LRCLK, 12 DOUT, 13 DIN, 14 MCLK.
    /// `wiring(pad)` gives the net id each fixture pad of each part is on.
    fn audio_board(parts: &[(&str, &str, [i64; 5])]) -> ExtractedBoard {
        let nets = &[
            (1, "GND"),
            (2, "+3V3"),
            (10, "I2S_BCLK"),
            (11, "I2S_LRCLK"),
            (12, "I2S_DOUT"),
            (13, "I2S_DIN"),
            (14, "I2S_MCLK"),
        ];
        let comps = parts
            .iter()
            .map(|(reference, value, w)| {
                part(
                    reference,
                    value,
                    &[
                        ("1", w[0]),
                        ("2", w[1]),
                        ("3", w[2]),
                        ("4", w[3]),
                        ("5", w[4]),
                        ("6", 2),
                        ("7", 1),
                    ],
                )
            })
            .collect();
        board(nets, comps)
    }

    fn run(b: &ExtractedBoard, lib: &ModelLibrary) -> Vec<LintFinding> {
        audio_bus_lint(b, lib).findings
    }

    /// Only the findings on the `I2S_DOUT` net: the boards that wire two
    /// DOUT pads together also leave `I2S_DIN` without a transmitter, which
    /// is a separate (correct) finding these tests are not about.
    fn dout(b: &ExtractedBoard, lib: &ModelLibrary) -> Vec<LintFinding> {
        let mut f = run(b, lib);
        f.retain(|f| f.nets.iter().any(|n| n == "I2S_DOUT"));
        f
    }

    /// The wiring of a controller master and a codec slave: BCLK, LRCLK and
    /// MCLK from the controller; the controller's DOUT into the codec's DIN
    /// and the codec's DOUT into the controller's DIN.
    const CTL_WIRES: [i64; 5] = [10, 11, 13, 12, 14];
    const CODEC_WIRES: [i64; 5] = [10, 11, 12, 13, 14];

    #[test]
    fn a_correctly_wired_bus_is_silent_in_both_master_directions() {
        // Controller master, codec slave.
        let lib = cards(
            "i2s_clock_role = \"master\"",
            "i2s_clock_role = \"slave\"\nmclk_required = true",
            "",
            "ok1",
        );
        let b = audio_board(&[("U1", "CTL", CTL_WIRES), ("U2", "CODEC", CODEC_WIRES)]);
        assert!(run(&b, &lib).is_empty(), "{:?}", run(&b, &lib));
        // The roles exchanged: codec master, controller slave.
        let lib = cards(
            "i2s_clock_role = \"slave\"",
            "i2s_clock_role = \"master\"",
            "",
            "ok2",
        );
        assert!(run(&b, &lib).is_empty(), "{:?}", run(&b, &lib));
        // Roles the cards do not state: nothing to hold the board to.
        let lib = cards("", "i2s_clock_role = \"configurable\"", "", "ok3");
        assert!(run(&b, &lib).is_empty(), "{:?}", run(&b, &lib));
    }

    #[test]
    fn two_transmitters_on_one_data_line_fire() {
        // Both DOUT pads (and no DIN) on I2S_DOUT.
        let lib = cards("", "", "", "dd");
        let b = audio_board(&[
            ("U1", "CTL", [10, 11, 13, 12, 14]),
            ("U2", "CODEC", [10, 11, 13, 12, 14]),
        ]);
        let f = run(&b, &lib);
        let dout: Vec<_> = f.iter().filter(|f| f.nets == ["I2S_DOUT"]).collect();
        assert_eq!(dout.len(), 1, "{f:?}");
        assert_eq!(dout[0].check, LintCheck::AudioBusWiring);
        assert_eq!(dout[0].severity, Severity::Medium);
        assert!(
            dout[0].message.contains("U1 pad 4 and U2 pad 4"),
            "{}",
            dout[0].message
        );
        assert!(
            dout[0].refs.contains(&"U1".to_string()) && dout[0].refs.contains(&"U2".to_string())
        );
    }

    #[test]
    fn a_tdm_line_shared_by_tristating_outputs_is_silent() {
        let b = audio_board(&[
            ("U1", "CTL", [10, 11, 13, 12, 14]),
            ("U2", "CODEC", [10, 11, 13, 12, 14]),
        ]);
        // Both cards say the output tri-states outside its slot.
        let lib = cards("dout_hiz = true", "dout_hiz = true", "", "tdm1");
        assert!(dout(&b, &lib).is_empty(), "{:?}", dout(&b, &lib));
        // Three transmitters on a line is a TDM bus.
        let lib = cards("", "", "", "tdm2");
        let b3 = audio_board(&[
            ("U1", "CTL", [10, 11, 13, 12, 14]),
            ("U2", "CODEC", [10, 11, 13, 12, 14]),
            ("U3", "ADC", [10, 11, 13, 12, 14]),
        ]);
        assert!(dout(&b3, &lib).is_empty(), "{:?}", dout(&b3, &lib));
        // A net named for TDM.
        let mut named = b;
        named.nets[4].name = "TDM_DOUT".into();
        let on_tdm: Vec<_> = run(&named, &lib)
            .into_iter()
            .filter(|f| f.nets.iter().any(|n| n == "TDM_DOUT"))
            .collect();
        assert!(on_tdm.is_empty(), "{on_tdm:?}");
    }

    #[test]
    fn two_receivers_and_no_transmitter_fire() {
        let lib = cards("", "", "", "dinin");
        // Both DIN pads (3) on I2S_DIN; nothing's DOUT goes there.
        let b = audio_board(&[
            ("U1", "CTL", [10, 11, 13, 12, 14]),
            ("U2", "CODEC", [10, 11, 13, 15, 14]),
        ]);
        let mut b = b;
        b.nets.push(hauksbee_extract::Net::new(15, "NC_OUT"));
        let f = run(&b, &lib);
        let din: Vec<_> = f.iter().filter(|f| f.nets == ["I2S_DIN"]).collect();
        // U1's DOUT is on I2S_DOUT alone, U2's DOUT on NC_OUT alone, so the
        // I2S_DIN line holds two data inputs and no output.
        assert_eq!(din.len(), 1, "{f:?}");
        assert!(
            din[0].message.contains("nothing transmits"),
            "{}",
            din[0].message
        );
    }

    #[test]
    fn crossed_clocks_fire() {
        let lib = cards("", "", "", "cross");
        // U2's LRCLK on the BCLK net.
        let b = audio_board(&[
            ("U1", "CTL", CTL_WIRES),
            ("U2", "CODEC", [10, 10, 12, 13, 14]),
        ]);
        let f = run(&b, &lib);
        let crossed: Vec<_> = f.iter().filter(|f| f.nets == ["I2S_BCLK"]).collect();
        assert_eq!(crossed.len(), 1, "{f:?}");
        assert_eq!(crossed[0].severity, Severity::High);
        assert!(
            crossed[0].message.contains("bit clock") && crossed[0].message.contains("frame clock"),
            "{}",
            crossed[0].message
        );
    }

    #[test]
    fn a_clock_with_two_masters_or_none_fires_only_on_stated_roles() {
        let b = audio_board(&[("U1", "CTL", CTL_WIRES), ("U2", "CODEC", CODEC_WIRES)]);
        // Both cards say master: two drivers on BCLK and LRCLK.
        let both = cards(
            "i2s_clock_role = \"master\"",
            "i2s_clock_role = \"master\"",
            "",
            "m2",
        );
        let f = run(&b, &both);
        assert_eq!(
            f.iter().filter(|f| f.nets == ["I2S_BCLK"]).count(),
            1,
            "{f:?}"
        );
        assert!(f.iter().all(|f| f.severity == Severity::High));
        // Both slaves: nobody clocks.
        let none = cards(
            "i2s_clock_role = \"slave\"",
            "i2s_clock_role = \"slave\"",
            "",
            "m0",
        );
        let f = run(&b, &none);
        assert!(
            f.iter().any(|f| f.message.contains("nothing drives it")),
            "{f:?}"
        );
        // One master, one unstated: legal. Both unstated: not judged.
        let one = cards("i2s_clock_role = \"master\"", "", "", "m1");
        assert!(run(&b, &one).is_empty());
        let unk = cards("", "", "", "mu");
        assert!(run(&b, &unk).is_empty());
    }

    #[test]
    fn a_codec_that_needs_mclk_with_none_connected_fires_and_an_optional_one_does_not() {
        // U2's MCLK pad (5) on its own net with no driver.
        let mut b = audio_board(&[
            ("U1", "CTL", CTL_WIRES),
            ("U2", "CODEC", [10, 11, 12, 13, 15]),
        ]);
        b.nets.push(hauksbee_extract::Net::new(15, "CODEC_MCLK"));
        let needs = cards("", "mclk_required = true", "", "mc1");
        let f = run(&b, &needs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].severity, Severity::High);
        assert!(f[0].message.contains("U2 pad 5"), "{}", f[0].message);
        // The datasheet lets the part run without MCLK, or the card is silent.
        for (params, tag) in [("mclk_required = false", "mc2"), ("", "mc3")] {
            let lib = cards("", params, "", tag);
            assert!(run(&b, &lib).is_empty(), "{params}: {:?}", run(&b, &lib));
        }
        // MCLK that is driven is fine.
        let wired = audio_board(&[("U1", "CTL", CTL_WIRES), ("U2", "CODEC", CODEC_WIRES)]);
        assert!(run(&wired, &needs).is_empty());
    }

    #[test]
    fn a_net_with_an_unclassified_pin_or_a_series_resistor_is_not_judged() {
        let lib = cards("", "", "", "unc");
        // Two DOUT pads plus a third part the audio vocabulary does not cover.
        let mut b = audio_board(&[
            ("U1", "CTL", [10, 11, 13, 12, 14]),
            ("U2", "CODEC", [10, 11, 13, 12, 14]),
        ]);
        let mut mcu = part("U9", "SOMEMCU", &[]);
        mcu.pins = vec![Pin {
            number: "7".into(),
            net: Some(12),
            ..Default::default()
        }];
        b.components.push(mcu);
        assert!(dout(&b, &lib).is_empty(), "an unclassified pin: abstain");
        // A series resistor onto another signal net continues the line.
        let mut b = audio_board(&[
            ("U1", "CTL", [10, 11, 13, 12, 14]),
            ("U2", "CODEC", [10, 11, 13, 12, 14]),
        ]);
        b.nets.push(hauksbee_extract::Net::new(16, "DOUT_FAR"));
        b.components.push(Component {
            lib_id: "Device:R".into(),
            footprint: "R_0603".into(),
            ..part("R5", "33", &[("1", 12), ("2", 16)])
        });
        assert!(dout(&b, &lib).is_empty(), "a series resistor: abstain");
    }

    #[test]
    fn a_net_named_for_one_signal_with_another_signals_pad_fires_even_beside_unknown_pins() {
        let lib = cards("", "", "", "swap");
        // U2's BCLK pad (1) on the LRCLK net and U2's LRCLK pad on the BCLK net:
        // an unclassified MCU pin on both nets keeps the roles check silent.
        let mut b = audio_board(&[
            ("U1", "CTL", CTL_WIRES),
            ("U2", "CODEC", [11, 10, 12, 13, 14]),
        ]);
        for net in [10, 11] {
            let mut mcu = part("U9", "SOMEMCU", &[]);
            mcu.reference = format!("U9{net}");
            mcu.pins = vec![Pin {
                number: "7".into(),
                net: Some(net),
                ..Default::default()
            }];
            b.components.push(mcu);
        }
        let f = run(&b, &lib);
        let on = |n: &str| f.iter().filter(|f| f.nets == [n]).collect::<Vec<_>>();
        assert_eq!(on("I2S_BCLK").len(), 1, "{f:?}");
        assert_eq!(on("I2S_LRCLK").len(), 1, "{f:?}");
        let bclk = on("I2S_BCLK")[0];
        assert_eq!(bclk.severity, Severity::Medium);
        assert!(
            bclk.message.contains("U2 pad 2") && bclk.message.contains("frame clock"),
            "{}",
            bclk.message
        );
        assert_eq!(bclk.check, LintCheck::AudioBusWiring);
    }

    #[test]
    fn the_roles_finding_is_not_doubled_by_the_name_finding() {
        // All pins classified: the roles check says it once.
        let lib = cards("", "", "", "swap2");
        let b = audio_board(&[
            ("U1", "CTL", CTL_WIRES),
            ("U2", "CODEC", [10, 10, 12, 13, 14]),
        ]);
        let f = run(&b, &lib);
        assert_eq!(
            f.iter().filter(|f| f.nets == ["I2S_BCLK"]).count(),
            1,
            "{f:?}"
        );
    }

    #[test]
    fn a_net_with_a_generic_or_matching_name_is_silent() {
        let lib = cards("", "", "", "swap3");
        let mut b = audio_board(&[
            ("U1", "CTL", CTL_WIRES),
            ("U2", "CODEC", [11, 10, 12, 13, 14]),
        ]);
        // An unclassified pin on both nets keeps the roles check silent, so
        // only the name could speak; generic names say nothing about the bus.
        for net in [10, 11] {
            let mut mcu = part("U9", "SOMEMCU", &[]);
            mcu.reference = format!("U9{net}");
            mcu.pins = vec![Pin {
                number: "7".into(),
                net: Some(net),
                ..Default::default()
            }];
            b.components.push(mcu);
        }
        b.nets[2].name = "CLK_A".into();
        b.nets[3].name = "CLK_B".into();
        assert!(run(&b, &lib).is_empty(), "{:?}", run(&b, &lib));
        // Data nets are not compared by direction: U2's DOUT on the DIN net.
        let b = audio_board(&[
            ("U1", "CTL", CTL_WIRES),
            ("U2", "CODEC", [10, 11, 13, 12, 14]),
        ]);
        assert!(
            run(&b, &lib)
                .iter()
                .all(|f| !f.message.contains("looks swapped")),
            "{:?}",
            run(&b, &lib)
        );
    }
}
