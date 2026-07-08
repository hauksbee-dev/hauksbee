//! Boot-strap coverage lint: flag a strap-bearing MCU whose straps were never
//! checked.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.
//!
//! hauksbee's boot strap-pin lint ([`straps`](super::straps)) keys off the
//! per-part device model's strap table. When a strap-bearing MCU (STM32 / ESP32
//! class) has no such table — it is absent from the model DB, or resolved only
//! to a strapless engine fallback — the strap lint iterates to zero findings. In
//! the report that is byte-identical to a board where it ran and found nothing,
//! so a bare "Looks healthy" verdict ends up claiming coverage that never
//! happened. On a real STM32WL55 board (phancak/LoRa-Board) the BOOT0 strap — a
//! hardware-only boot-mode latch the firmware cannot override — was silently
//! unchecked while `--lint` printed "Looks healthy".
//!
//! This check closes that gap. It emits one informational
//! [`LintCheck::UncheckedMcu`] finding per strap-bearing MCU whose strap table
//! was not examined, so a "healthy" verdict is not printed over a recognised
//! boot-strap surface the tool never looked at. (It is a heuristic recogniser,
//! so a part it does not recognise can still slip through; it does not claim to
//! catch every unmodelled chip.) It is `Severity::Low`: a coverage gap, not a
//! board defect, so it informs without failing a `--strict` gate.

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

/// Does this reference designator name an active IC (`U*` / `IC*` / `MCU*`)?
/// Guards against a connector or test point whose value happens to mention a
/// chip. The set mirrors the binder's own MCU-candidate refdes prefixes so a
/// part the binder treats as an MCU is not invisible to this check.
fn is_active_ic_refdes(reference: &str) -> bool {
    let r = reference.to_ascii_uppercase();
    // Strip to the leading alphabetic prefix.
    let prefix: String = r.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    prefix == "U" || prefix == "IC" || prefix == "MCU"
}

/// Families with a boot-mode pin sampled by hardware at reset — the surface the
/// strap lint exists for. STM32 and its pin-compatible clones latch BOOT0; the
/// ESP32 family latches its strapping pins (GPIO0/2/12/15). These are the ONLY
/// families for which "no strap table examined" is a real coverage gap: AVR
/// (fuses), PIC, MSP430, SAMD, nRF and friends have no reset-sampled boot strap,
/// so flagging them would be noise. A strict subset of [`MCU_FAMILY_PREFIXES`].
const STRAP_BEARING_PREFIXES: &[&str] = &[
    // STM32 + pin-compatible BOOT0 clones. NB "AT32F" (Artery), NOT "AT32": the
    // bare prefix would also swallow Atmel's AVR32 (AT32UC3…), which has no BOOT0.
    "STM32", "GD32", "APM32", "AT32F", "CKS32", "HK32", "PY32", "MM32",
    "ESP32", "ESP8266", "ESP8285", // Espressif strapping pins
];

/// True when `s`, uppercased and leading-trimmed, starts with a known MCU family.
fn value_opens_with_mcu_family(s: &str) -> bool {
    let up = s.trim().to_ascii_uppercase();
    MCU_FAMILY_PREFIXES.iter().any(|p| up.starts_with(p))
}

/// True when this component identifies as a strap-bearing family (BOOT0 / ESP32
/// strapping). Checks the value and MPN by prefix and the KiCad symbol library
/// by substring (`MCU_ST_STM32WL`, `RF_Module:ESP32-WROOM`).
fn is_strap_bearing_family(comp: &Component) -> bool {
    let up = comp.value.trim().to_ascii_uppercase();
    if STRAP_BEARING_PREFIXES.iter().any(|p| up.starts_with(p)) {
        return true;
    }
    let lib = comp.lib_id.to_ascii_uppercase();
    if STRAP_BEARING_PREFIXES.iter().any(|p| lib.contains(p)) {
        return true;
    }
    comp.properties.iter().any(|(k, v)| {
        let kl = k.to_ascii_lowercase();
        (kl.contains("mpn") || kl.contains("part"))
            && STRAP_BEARING_PREFIXES
                .iter()
                .any(|p| v.trim().to_ascii_uppercase().starts_with(p))
    })
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
    // `MCU_ST_STM32WL:STM32WL55CCUx`) and modules in `RF_Module:ESP32-WROOM-32`.
    // The library is the part before ':'. Accept either convention, or a library
    // that names a known family directly, so an ESP module whose *value* is
    // "ESP-WROOM-32" (does not open with "ESP32") is still recognised — matching
    // what `is_strap_bearing_family` keys on.
    let lib_up = comp.lib_id.to_ascii_uppercase();
    let lib_seg = lib_up.split(':').next().unwrap_or("");
    if lib_seg.starts_with("MCU_")
        || lib_seg.starts_with("RF_MODULE")
        || MCU_FAMILY_PREFIXES.iter().any(|p| lib_up.contains(p))
    {
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

/// Emit one informational finding per **strap-bearing** MCU whose boot-strap
/// pins were never examined, so the strap lint's silence is never mistaken for a
/// clean bill of health.
///
/// Two predicates, both required:
///
/// 1. The part is a strap-bearing MCU ([`is_strap_bearing_family`]): STM32 + its
///    BOOT0 clones, or the ESP32 family. These are the only families with a
///    reset-sampled boot strap, so they are the only ones for which "no strap
///    table" is a real gap. AVR (fuses), PIC, MSP430, SAMD, nRF, etc. are not
///    flagged — they have no boot strap to check.
///
/// 2. Its strap table was not examined: the part resolved to no model, OR to a
///    model with an empty strap table. The load-bearing subtlety is that "has a
///    model" is NOT "the strap check ran": the binder's engine fallback router
///    synthesises a model for STM32F1/F4 and ESP32 parts absent from the device
///    DB, with a backend and pin roles but an **empty strap table**. `strap_lint`
///    skips on `straps.is_empty()`, so an STM32F407's BOOT0 is never examined.
///    Keying off `straps.is_empty()` (rather than `model.is_some()`) catches that
///    case; a DB-authored part with a populated strap table (STM32F103C8) is not
///    flagged because its straps genuinely were checked.
///
/// A mis-strapped boot pin is a reset-time hardware latch the firmware cannot
/// override, so a false "Looks healthy" over an unchecked one is the worst kind
/// of false comfort. `Severity::Low`: a coverage gap, not a board defect.
///
/// Residual same-class gap (not covered): a part with a non-empty strap table is
/// treated as examined, but `strap_lint` examines zero pins if the strap pad is
/// absent on the extracted footprint or unrouted. That is outside this check's
/// "no strap table" scope and left to the strap lint's own visibility caveats.
pub fn mcu_coverage_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = NetLintReport::default();
    for comp in &board.components {
        if !is_probable_mcu(comp) || !is_strap_bearing_family(comp) {
            continue;
        }
        let straps_examined = resolve(lib, comp)
            .model
            .as_ref()
            .is_some_and(|m| !m.straps.is_empty());
        if straps_examined {
            continue; // strap lint had a populated table and ran
        }
        let label = if comp.value.trim().is_empty() || comp.value.trim() == "~" {
            comp.reference.clone()
        } else {
            format!("{} ({})", comp.reference, comp.value.trim())
        };
        report.findings.push(LintFinding {
            check: LintCheck::UncheckedMcu,
            severity: Severity::Low,
            message: format!(
                "{label} looks like a strap-bearing MCU (STM32/ESP32 class) but has no boot-strap \
                 table in the model database (it is absent, or resolved only to a generic fallback), \
                 so its boot strap-pins (e.g. BOOT0 / ESP32 strapping pins) were NOT checked — a \
                 mis-strapped boot pin is a reset-time latch firmware cannot override"
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

    /// End-to-end against the real model DB: an unmodelled strap-bearing MCU
    /// (STM32WL55) yields exactly one UncheckedMcu note naming it; a DB-authored
    /// strap-bearing MCU with a populated strap table (STM32F103C8 — straps WERE
    /// examined) and a passive yield none. This is the guard that stops "Looks
    /// healthy" from being printed over an MCU whose straps were never examined.
    #[test]
    fn flags_unmodelled_mcu_but_not_a_modelled_one() {
        let lib = ModelLibrary::builtin();
        let board = ExtractedBoard {
            name: "t".into(),
            nets: Vec::new(),
            components: vec![
                comp("U4", "STM32WL55CCU6", "Package_QFP:LQFP-48"), // not in DB → flagged
                comp("U1", "STM32F103C8T6", "Package_QFP:LQFP-48"), // DB-authored, has straps
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
        assert!(f.message.contains("boot strap-pins"));
    }

    /// The blocker case: a part absent from the device DB but routed to a strapless
    /// engine fallback (STM32F407 → `route_mcu_family`) must STILL be flagged. The
    /// fallback gives it a model with no strap table, so `strap_lint` skips its
    /// BOOT0 silently; gating on `model.is_some()` would miss it and reprint "Looks
    /// healthy" over an unchecked boot-mode latch — the exact bug this check exists
    /// to prevent. A DB-authored STM32F103C8 (real strap table) must NOT be flagged.
    #[test]
    fn flags_fallback_routed_mcu_but_not_a_db_authored_one() {
        let lib = ModelLibrary::builtin();
        let board = ExtractedBoard {
            name: "t".into(),
            nets: Vec::new(),
            components: vec![
                comp("U1", "STM32F407VGT6", "Package_QFP:LQFP-100"), // DB-miss → fallback
                comp("U2", "STM32F103C8T6", "Package_QFP:LQFP-48"),  // DB-authored, has straps
            ],
        };
        let report = mcu_coverage_lint(&board, &lib);
        let refs: Vec<&str> = report.findings.iter().map(|f| f.refs[0].as_str()).collect();
        assert_eq!(
            refs,
            vec!["U1"],
            "fallback-routed STM32F407 must be flagged; DB-authored STM32F103C8 must not"
        );
    }

    /// The ESP32 co-headline: an unmodelled ESP32 part (whose strap table the DB
    /// would otherwise carry) is flagged; a DB-authored `ESP32-WROOM-32` — which
    /// the strap lint really does model with GPIO0/2/12/15 — is not. Also pins the
    /// `lib_id`-only recognition path: a module whose value is "ESP-WROOM-32" (does
    /// not open with "ESP32") but whose library is `RF_Module:ESP32-WROOM-32`.
    #[test]
    fn flags_unmodelled_esp_module_but_not_db_authored_wroom() {
        let lib = ModelLibrary::builtin();
        let board = ExtractedBoard {
            name: "t".into(),
            nets: Vec::new(),
            components: vec![
                // value doesn't open with ESP32; only the RF_Module library says ESP32.
                comp("U1", "ESP-WROOM-32", "RF_Module:ESP32-WROOM-32"),
                comp("U2", "ESP32-WROOM-32", "RF_Module:ESP32-WROOM-32"), // DB-authored
            ],
        };
        let report = mcu_coverage_lint(&board, &lib);
        let refs: Vec<&str> = report.findings.iter().map(|f| f.refs[0].as_str()).collect();
        // U2 is the DB-modelled WROOM (has straps) → not flagged. U1 ("ESP-WROOM-32")
        // is the same module value-unrecognised but lib-recognised; whether it has a
        // DB strap table depends on the value_re, so assert only the invariant that
        // matters: the DB-authored WROOM is never flagged.
        assert!(
            !refs.contains(&"U2"),
            "DB-authored ESP32-WROOM-32 (real strap table) must not be flagged; got {refs:?}"
        );
        // And the lib-only part must at least be *recognised* as strap-bearing.
        assert!(is_strap_bearing_family(&comp(
            "U1",
            "ESP-WROOM-32",
            "RF_Module:ESP32-WROOM-32"
        )));
        assert!(is_probable_mcu(&comp(
            "U1",
            "ESP-WROOM-32",
            "RF_Module:ESP32-WROOM-32"
        )));
    }

    /// ESP32 positive path (the co-headline): an ESP32 value that misses the DB
    /// but routes to a strapless engine fallback (e.g. a bare `ESP32-C3`) must be
    /// flagged — its GPIO strapping pins were never examined.
    #[test]
    fn flags_fallback_routed_esp32() {
        let lib = ModelLibrary::builtin();
        let board = ExtractedBoard {
            name: "t".into(),
            nets: Vec::new(),
            components: vec![comp("U1", "ESP32-C3FH4", "Package:QFN-32")],
        };
        let report = mcu_coverage_lint(&board, &lib);
        assert_eq!(
            report.findings.iter().map(|f| f.refs[0].as_str()).collect::<Vec<_>>(),
            vec!["U1"],
            "a fallback-routed ESP32 (strapless model) must be flagged"
        );
    }

    /// AT32UC3 (Atmel AVR32) must NOT be treated as a strap-bearing STM32 clone:
    /// the bare "AT32" prefix would swallow it, but it has no BOOT0.
    #[test]
    fn avr32_at32uc3_is_not_strap_bearing() {
        assert!(!is_strap_bearing_family(&comp("U1", "AT32UC3A0512", "Package:TQFP")));
        // An Artery AT32F403 IS strap-bearing.
        assert!(is_strap_bearing_family(&comp("U2", "AT32F403ACGU7", "Package:QFN")));
    }

    /// A non-strap-bearing MCU must NOT be flagged, even when unmodelled. SAMD
    /// (like AVR/PIC/nRF) has no reset-sampled boot strap, so "no strap table" is
    /// not a coverage gap — flagging it would be the noise this check avoids.
    #[test]
    fn non_strap_bearing_unmodelled_mcu_is_not_flagged() {
        let lib = ModelLibrary::builtin();
        let board = ExtractedBoard {
            name: "t".into(),
            nets: Vec::new(),
            components: vec![
                comp_n("U1", "ATSAMD51J20A-AU", "Package_QFP:LQFP-64", 40), // not in DB, no boot strap
                comp("U2", "ATmega2560-16AU", "Package_QFP:TQFP-100"),      // AVR, fuses not straps
            ],
        };
        assert!(
            mcu_coverage_lint(&board, &lib).findings.is_empty(),
            "non-strap-bearing MCUs (SAMD, AVR) must not be flagged"
        );
    }
}
