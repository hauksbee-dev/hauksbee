//! MCU coverage lint: flag a component that looks like an MCU but has no model.
//!
//! hauksbee's two *differentiated* lint checks — the boot strap-pin lint
//! ([`straps`](super::straps)) and the internal resource-conflict check — both
//! key off the per-part device model. When the MCU is not in the model DB it
//! binds open and BOTH checks iterate to zero findings. In the report that is
//! byte-identical to a board where the checks ran and found nothing, so a bare
//! "Looks healthy" verdict ends up claiming coverage that never happened. On a
//! real STM32WL55 board (phancak/LoRa-Board) the BOOT0 strap — a hardware-only
//! boot-mode latch the firmware cannot override — was silently unchecked while
//! `--lint` printed "Looks healthy".
//!
//! This check closes that gap. It emits one informational
//! [`LintCheck::UncheckedMcu`] finding per component that *looks like an MCU*
//! (active-IC reference designator + a recognised MCU part-number family, or
//! KiCad's `MCU_*` symbol-library convention) yet resolved to no model. The
//! finding names the part and exactly which checks were skipped, so a "healthy"
//! verdict is never printed over an MCU the tool never looked at. It is
//! `Severity::Low`: an unmodelled part is a coverage gap, not a board defect, so
//! it informs without failing a `--strict` gate.

use hauksbee_extract::{Component, ExtractedBoard, LintCheck, LintFinding, NetLintReport, Severity};
use hauksbee_models::ModelLibrary;

use crate::binder::resolve;

/// Recognised MCU part-number family prefixes (compared case-insensitively
/// against the value and any property string, which must *start with* the
/// prefix). Deliberately a fixed, auditable list of well-known families rather
/// than a fuzzy regex: it fires only on a string that genuinely opens with an
/// MCU family, so a datasheet URL or a description that merely contains the name
/// does not trip it.
const MCU_FAMILY_PREFIXES: &[&str] = &[
    // ARM Cortex-M from ST and pin-compatible clones, plus 8-bit STM8.
    "STM32", "STM8", "GD32", "APM32", "AT32", "PY32", "CKS32", "HK32",
    // Atmel/Microchip AVR.
    "ATMEGA", "ATTINY", "ATXMEGA", "AT90",
    // Microchip SAM (Cortex-M).
    "ATSAM", "SAMD", "SAML", "SAME", "SAMC", "SAMG",
    // Espressif.
    "ESP32", "ESP8266", "ESP8285",
    // Raspberry Pi.
    "RP2040", "RP2350",
    // Nordic.
    "NRF51", "NRF52", "NRF53", "NRF54", "NRF91",
    // TI MSP.
    "MSP430", "MSPM0", "MSP432",
    // Microchip PIC / dsPIC.
    "PIC10", "PIC12", "PIC16", "PIC18", "PIC24", "PIC32", "DSPIC",
    // NXP LPC / Kinetis / i.MX RT.
    "LPC8", "LPC11", "LPC15", "LPC17", "LPC18", "LPC40", "LPC43", "LPC54", "LPC55",
    "MK20", "MK22", "MK64", "MK66", "MKL", "MKE", "MKW", "IMXRT", "MIMXRT", "S32K",
    // Silicon Labs.
    "EFM32", "EFR32",
    // WCH.
    "CH32", "CH56", "CH57", "CH58", "CH59",
    // Renesas RA, RISC-V, Nuvoton, misc.
    "RA2", "RA4", "RA6", "FE310", "GD32VF", "NUC1", "MM32", "HT32", "HC32",
];

/// Does this reference designator name an active IC (`U*` / `IC*`)? Guards
/// against a connector or test point whose value happens to mention a chip.
fn is_active_ic_refdes(reference: &str) -> bool {
    let r = reference.to_ascii_uppercase();
    // Strip to the leading alphabetic prefix.
    let prefix: String = r.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    prefix == "U" || prefix == "IC"
}

/// True when `s`, uppercased and leading-trimmed, starts with a known MCU family.
fn value_opens_with_mcu_family(s: &str) -> bool {
    let up = s.trim().to_ascii_uppercase();
    MCU_FAMILY_PREFIXES.iter().any(|p| up.starts_with(p))
}

/// Heuristic: does this component look like an MCU? Conservative by design —
/// it must be an active-IC designator AND carry an MCU signature in its value,
/// a property string, or KiCad's `MCU_*` symbol library id.
pub(crate) fn is_probable_mcu(comp: &Component) -> bool {
    if comp.dnp {
        return false; // not assembled → electrically absent
    }
    if !is_active_ic_refdes(&comp.reference) {
        return false;
    }
    // KiCad puts MCU symbols in libraries named `MCU_*` (e.g.
    // `MCU_ST_STM32WL:STM32WL55CCUx`). The library is the part before ':'.
    let lib = comp.lib_id.split(':').next().unwrap_or("");
    if lib.to_ascii_uppercase().starts_with("MCU_") {
        return true;
    }
    if value_opens_with_mcu_family(&comp.value) {
        return true;
    }
    // Only consult part-number-like properties (MPN / "Part Number"), never free
    // text such as Datasheet or Description — a regulator whose description opens
    // with "STM32-compatible …" must not be mistaken for an MCU.
    comp.properties.iter().any(|(k, v)| {
        let kl = k.to_ascii_lowercase();
        (kl.contains("mpn") || kl.contains("part")) && value_opens_with_mcu_family(v)
    })
}

/// Emit one informational finding per probable-MCU component that bound open (no
/// device model), so the model-driven checks' silence is never mistaken for a
/// clean bill of health.
///
/// Scope is deliberately the *fully unmodelled* MCU — the airtight case where the
/// part isn't in the model DB at all, so `--report` itself calls it "active IC
/// left OPEN; NOT trustworthy". The boot strap-pin lint, which needs the model's
/// strap table, definitely did not examine it. The resource-conflict check is
/// matched independently from `mcu_resources.toml`, so it *may* still have run
/// (e.g. an unmodelled SAMD51 is resource-mapped); the message names the resource
/// surface only when that check truly did not cover the part. A *modelled* MCU
/// that merely lacks a resource map is not flagged: hauksbee ships resource maps
/// for only a handful of MCUs by design, so treating their absence as a coverage
/// gap would fire on nearly every board and breach the zero-false-positive bar.
pub fn mcu_coverage_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = NetLintReport::default();
    let resource_checked = board.resource_checked_refs();
    for comp in &board.components {
        if !is_probable_mcu(comp) {
            continue;
        }
        if resolve(lib, comp).model.is_some() {
            continue; // modelled → the strap lint had a table and ran
        }
        let label = if comp.value.trim().is_empty() || comp.value.trim() == "~" {
            comp.reference.clone()
        } else {
            format!("{} ({})", comp.reference, comp.value.trim())
        };
        // The strap check always misses an unmodelled MCU (no strap table). The
        // resource check is independent, so only name it when it really skipped.
        let skipped = if resource_checked.contains(&comp.reference) {
            "its boot strap-pins (e.g. BOOT0/NRST) were NOT checked".to_string()
        } else {
            "its boot strap-pins (e.g. BOOT0/NRST) and internal resource conflicts were NOT checked"
                .to_string()
        };
        report.findings.push(LintFinding {
            check: LintCheck::UncheckedMcu,
            severity: Severity::Low,
            message: format!(
                "{label} looks like an MCU but is not in the model database, so it bound open: {skipped}"
            ),
            refs: vec![comp.reference.clone()],
            nets: Vec::new(),
        });
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use hauksbee_extract::{Component, Pin};

    fn comp(reference: &str, value: &str, lib_id: &str) -> Component {
        comp_n(reference, value, lib_id, 1)
    }

    /// A component with `n` connected pins (numbered 1..=n), so a test can clear
    /// the resource tables' `min_pins` guard.
    fn comp_n(reference: &str, value: &str, lib_id: &str, n: usize) -> Component {
        Component {
            reference: reference.to_string(),
            value: value.to_string(),
            lib_id: lib_id.to_string(),
            footprint: String::new(),
            position: None,
            layer: String::new(),
            properties: Vec::new(),
            dnp: false,
            pins: (1..=n)
                .map(|i| Pin {
                    number: i.to_string(),
                    net: Some(0),
                    function: String::new(),
                    kind: String::new(),
                    position: None,
                })
                .collect(),
        }
    }

    #[test]
    fn recognises_mcu_by_value_family() {
        assert!(is_probable_mcu(&comp("U4", "STM32WL55CCU6", "Package:QFN")));
        assert!(is_probable_mcu(&comp("U1", "ATmega328P-AU", "Package:TQFP")));
        assert!(is_probable_mcu(&comp("IC2", "ESP32-S3", "Module:WROOM")));
        assert!(is_probable_mcu(&comp("U7", "RP2040", "Package:QFN-56")));
        assert!(is_probable_mcu(&comp("U9", "STM8S003F3", "Package:TSSOP")));
    }

    #[test]
    fn recognises_mcu_by_mpn_property_but_not_freetext() {
        let mut by_mpn = comp("U5", "~", "Package:QFN");
        by_mpn.properties = vec![("MPN".into(), "STM32G030F6".into())];
        assert!(is_probable_mcu(&by_mpn));

        // A description/datasheet that merely opens with a family name is not an MPN.
        let mut by_desc = comp("U6", "buck reg", "Package:QFN");
        by_desc.properties = vec![("Description".into(), "STM32-compatible level shifter".into())];
        assert!(!is_probable_mcu(&by_desc));
    }

    #[test]
    fn recognises_mcu_by_kicad_symbol_library() {
        // Odd / empty value but the MCU_* symbol library is authoritative.
        assert!(is_probable_mcu(&comp("U2", "~", "MCU_ST_STM32WL:STM32WL55CCUx")));
    }

    #[test]
    fn does_not_flag_passives_or_connectors() {
        assert!(!is_probable_mcu(&comp("R1", "10k", "Device:R")));
        assert!(!is_probable_mcu(&comp("C5", "100n", "Device:C")));
        // A connector whose value mentions a chip must not trip the active-IC guard.
        assert!(!is_probable_mcu(&comp("J2", "STM32_DEBUG_HDR", "Connector:Conn_01x04")));
        // A description that merely *contains* a family name (not at the start).
        assert!(!is_probable_mcu(&comp("U9", "Level shifter for STM32", "Package:SOT")));
    }

    #[test]
    fn dnp_part_is_not_flagged() {
        let mut c = comp("U4", "STM32WL55CCU6", "Package:QFN");
        c.dnp = true;
        assert!(!is_probable_mcu(&c));
    }

    /// End-to-end against the real model DB: an unmodelled MCU (STM32WL55) yields
    /// exactly one UncheckedMcu note naming it; a modelled MCU (ATmega328P) and a
    /// passive yield none. This is the guard that stops "Looks healthy" from being
    /// printed over an MCU the strap/resource checks never examined.
    #[test]
    fn flags_unmodelled_mcu_but_not_a_modelled_one() {
        let lib = ModelLibrary::builtin();
        let board = ExtractedBoard {
            name: "t".into(),
            nets: Vec::new(),
            components: vec![
                comp("U4", "STM32WL55CCU6", "Package_QFP:LQFP-48"), // not in DB
                comp("U1", "ATmega328P-AU", "Package_QFP:TQFP-32"), // in DB
                comp("R1", "10k", "Device:R"),                      // passive
            ],
        };
        let report = mcu_coverage_lint(&board, &lib);
        assert_eq!(report.findings.len(), 1, "exactly one unchecked-MCU note");
        let f = &report.findings[0];
        assert_eq!(f.check, LintCheck::UncheckedMcu);
        assert_eq!(f.severity, Severity::Low);
        assert_eq!(f.refs, vec!["U4".to_string()]);
        assert!(f.message.contains("STM32WL55CCU6"));
        // STM32WL55 has no resource table, so the note must name the resource
        // surface as also unchecked.
        assert!(
            f.message.contains("internal resource conflicts"),
            "unmapped MCU note must say resources were not checked: {}",
            f.message
        );
    }

    /// The message must NOT claim the resource-conflict check was skipped when it
    /// actually ran. An unmodelled ATSAMD51 (absent from the device-model DB) is
    /// still matched by the independent resource map, so its note names ONLY the
    /// strap surface — this is the factual-accuracy guard the first cut failed.
    #[test]
    fn resource_mapped_unmodelled_mcu_does_not_claim_resources_skipped() {
        let lib = ModelLibrary::builtin();
        // 40 pins clears the samd51j_tqfp64 table's min_pins guard.
        let board = ExtractedBoard {
            name: "t".into(),
            nets: Vec::new(),
            components: vec![comp_n("U1", "ATSAMD51J20A-AU", "Package_QFP:LQFP-64", 40)],
        };
        let report = mcu_coverage_lint(&board, &lib);
        assert_eq!(report.findings.len(), 1);
        let msg = &report.findings[0].message;
        assert!(msg.contains("boot strap-pins"), "names the real gap: {msg}");
        assert!(
            !msg.contains("internal resource conflicts"),
            "must not claim the resource check was skipped (it ran via the resource map): {msg}"
        );
    }
}
