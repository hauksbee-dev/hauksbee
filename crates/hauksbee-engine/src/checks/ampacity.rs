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

/// Attribute cited currents to nets from the bound DB models. Returns a map of
/// `net name -> (current, citation)` in the shape [`audit_trace_currents`] wants.
fn attribute_currents(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
) -> HashMap<String, (f64, String)> {
    let mut out: HashMap<String, Attribution> = HashMap::new();

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

    out.into_iter()
        .map(|(k, a)| (k, (a.current_a, a.citation)))
        .collect()
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
/// terminals, and a rating with no pin map is the old conservative behaviour.
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
    let cited = attribute_currents(board, lib);
    if cited.is_empty() {
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

    use hauksbee_extract::{Net, Pin};

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
