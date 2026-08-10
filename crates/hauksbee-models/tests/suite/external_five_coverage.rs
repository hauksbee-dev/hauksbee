//! The parts the first external-five gate could not name.
//!
//! Five real, published boards were run through the library and every part it
//! could not resolve was written down. This file pins the entries that answered
//! that list, and it is organised by the FAILURE MODE each one fixed rather than
//! by vendor, because the failure modes are what generalise:
//!
//!   1. A part number the library had, under a spelling no CAD tool writes
//!      (Eagle's "ATMEGA1284M" for the 44-pad ATmega1284P).
//!   2. A module, where the whole board hangs off one carrier (the Pro Micro on
//!      a split keyboard) and the header pin numbers are not the chip's.
//!   3. A part number that ENCODES the parameter, where matching the family
//!      would regulate the wrong rail (the LDO voltage codes).
//!   4. A bare, ungraded part number where the library only had a graded bin
//!      (BC807 against BC807-40, a factor of 1.8 in beta).
//!   5. A pin map already in the library that was simply WRONG: the 74LVC1G66's
//!      pads 2 and 3 were transposed, so a switch was stamped onto the ground
//!      plane on every board carrying the part.
//!
//! What every test here shares is that it asserts the thing that would silently
//! be WRONG rather than merely absent, because absent is the safe failure and
//! this library's job is to avoid the other one.

use hauksbee_models::{ComponentKind, ComponentQuery, ModelLibrary};

/// Resolve exactly the way a layout-only board does. `mpn` is set to the value
/// string as well as `value`, because that is what `binder.rs` does for a
/// component with no MPN property, which is every component on a `.kicad_pcb`,
/// a `.brd` or a gerber set. Leaving `mpn` empty here would make every entry that
/// carries an `mpn_re` alongside its `value_re` resolve to nothing (the rules are
/// ANDed), and this file would then be testing a query shape no board produces.
fn resolve(value: &str) -> Option<hauksbee_models::ModelEntry> {
    let lib = ModelLibrary::builtin();
    lib.resolve(&ComponentQuery {
        value: Some(value.to_string()),
        mpn: Some(value.to_string()),
        ..Default::default()
    })
    .model
}

fn expect(value: &str) -> hauksbee_models::ModelEntry {
    resolve(value).unwrap_or_else(|| panic!("{value} did not resolve"))
}

fn expect_id(value: &str, id: &str) -> hauksbee_models::ModelEntry {
    let m = expect(value);
    assert_eq!(m.id, id, "{value} resolved to the wrong entry");
    m
}

fn assert_pins(m: &hauksbee_models::ModelEntry, pairs: &[(&str, &str)]) {
    for (pad, role) in pairs {
        assert_eq!(
            m.pins.get(*pad).map(String::as_str),
            Some(*role),
            "{}: pad {pad} must be {role} (got {:?})",
            m.id,
            m.pins.get(*pad)
        );
    }
}

// ── 1. The spelling no CAD tool writes ───────────────────────────────────────

/// Eagle's Atmel library names the 44-pad MLF part "ATMEGA1284M", with the
/// package as a trailing letter, and that is the literal string in the value
/// field of every board laid out with it. The library had no ATmega1284 entry at
/// all, so a datalogger built around one reported its own processor as an open
/// circuit.
///
/// The pin map is the load-bearing assertion. A 1284P is NOT a 328P with more
/// flash: pad 1 is MOSI here and PC6/RESET on a 328P, so binding the board
/// through the 328P map would wire the SPI bus to the reset pin.
#[test]
fn the_eagle_mlf44_spelling_of_the_atmega1284p_binds_to_its_own_pinout() {
    // Including the two CAD-library device names, whose unhyphenated trailing
    // letter is a package code and cannot be part of an Atmel ordering code.
    for value in [
        "ATMEGA1284M",
        "ATMEGA1284A",
        "ATmega1284P",
        "ATmega1284P-AU",
        "ATmega1284P-MUR",
    ] {
        let m = expect_id(value, "atmega1284p");
        assert_eq!(m.kind, ComponentKind::Mcu);
        assert_eq!(
            m.params.get_str("backend"),
            Some("simavr:atmega1284p"),
            "{value}: simavr models this core directly; no substitution"
        );
    }

    // The non-P part is a SEPARATE simavr core and therefore a separate entry.
    // Folding it into the P entry would run its firmware on a P core with no
    // warning anywhere, because `detect_substitution` returns None for every
    // simavr backend by design.
    for value in ["ATmega1284", "ATmega1284-AU", "ATmega1284-MUR"] {
        let m = expect_id(value, "atmega1284");
        assert_eq!(m.params.get_str("backend"), Some("simavr:atmega1284"));
    }
    assert_eq!(
        resolve("ATmega1284").map(|m| m.id),
        Some("atmega1284".to_string()),
        "the bare non-P part must not land on the P entry"
    );

    let m = expect("ATMEGA1284M");
    // Atmel-42719C section 5.1.2, the four corners of the 44-pad map plus every
    // supply and the crystal pair. Cross-checked against the margay datalogger's
    // own net names, which agree pad for pad.
    assert_pins(
        &m,
        &[
            ("1", "pb5_mosi"),
            ("2", "pb6_miso"),
            ("3", "pb7_sck"),
            ("4", "reset"),
            ("5", "vcc"),
            ("6", "gnd"),
            ("7", "xtal2"),
            ("8", "xtal1"),
            ("19", "pc0_scl"),
            ("20", "pc1_sda"),
            ("27", "avcc"),
            ("29", "aref"),
            ("37", "pa0_adc0"),
            ("44", "pb4_ss_oc0b"),
            ("TH", "gnd5"),
        ],
    );
    assert_eq!(m.pins.len(), 45, "44 leads plus the exposed pad");

    // Table 29-1: the same 6 V / 40 mA / 200 mA envelope as the 328P, which is
    // what lets the supply-rail stress watch judge a board that overfeeds it.
    assert_eq!(m.ratings.max_voltage_v, Some(6.0));
    assert_eq!(m.ratings.max_pin_current_a, Some(0.04));
    assert_eq!(m.ratings.max_current_a, Some(0.2));

    // A 328P must not reach the 1284P entry, and the wider megaX4 siblings must
    // not borrow this pad count.
    for value in ["ATmega328P", "ATmega644P", "ATmega164P", "ATmega1281"] {
        assert!(
            resolve(value).is_none_or(|m| m.id != "atmega1284p"),
            "{value} must not resolve to the ATmega1284P entry"
        );
    }
}

// ── 2. The module whose header numbers are not the chip's ────────────────────

/// A split keyboard is a switch matrix and one Pro Micro, so that single part is
/// the difference between a board that simulates and a board with no processor.
///
/// The roles are the assertion, not just the presence. The binder's `module`
/// d-number table is the Arduino NANO's ATmega328P mapping, and the silkscreened
/// numbers do not agree between the two boards: D5 is PC6 on a Pro Micro and PD5
/// on a Nano, D8 is PB4 here and PB0 there. So this entry names the PORT PIN on
/// every header position, and a regression that replaced these with `d<n>` names
/// would wire every key row to the wrong port bit while still resolving.
#[test]
fn the_pro_micro_header_names_port_pins_not_arduino_numbers() {
    for value in [
        "ProMicro",
        "Pro Micro",
        "Pro-Micro",
        "SparkFun Pro Micro",
        "pro_micro",
        "ProMicro 5V",
        "ProMicro-16MHz",
    ] {
        let m = expect_id(value, "pro_micro");
        assert_eq!(m.kind, ComponentKind::Mcu);
        assert_eq!(m.params.get_str("backend"), Some("simavr:atmega32u4"));
        assert_eq!(
            m.params.get_bool("module"),
            Some(true),
            "{value}: the pin map is at the header, not the chip"
        );
    }

    let m = expect("ProMicro");
    // Header position -> ATmega32U4 port pin. Corroborated against the Sofle v2
    // keyboard's own symbol, which spells the port on every pin.
    assert_pins(
        &m,
        &[
            ("1", "pd3_txd1"),
            ("2", "pd2_rxd1"),
            ("3", "gnd"),
            ("4", "gnd2"),
            ("5", "pd1_sda"),
            ("6", "pd0_scl"),
            ("7", "pd4_adc8"),
            ("8", "pc6"),
            ("9", "pd7_adc10"),
            ("10", "pe6"),
            ("11", "pb4_adc11"),
            ("12", "pb5_adc12"),
            ("13", "pb6_adc13"),
            ("14", "pb2_mosi"),
            ("15", "pb3_miso"),
            ("16", "pb1_sck"),
            ("17", "pf7_adc7"),
            ("18", "pf6_adc6"),
            ("19", "pf5_adc5"),
            ("20", "pf4_adc4"),
            ("21", "vcc"),
            ("22", "reset"),
            ("23", "gnd3"),
            ("24", "raw"),
        ],
    );
    assert_eq!(m.pins.len(), 24, "24 header positions");

    // Not one role may be an Arduino d-number: that would route through the
    // Nano's table. This is the assertion the whole entry turns on.
    for (pad, role) in &m.pins {
        let is_arduino_number = role
            .strip_prefix('d')
            .is_some_and(|rest| rest.split('_').next().unwrap_or("").parse::<u8>().is_ok());
        assert!(
            !is_arduino_number,
            "pad {pad} is named {role}: an Arduino d-number on a module routes \
             through the ATmega328P Nano table, which is the wrong chip"
        );
    }

    // RAW is ahead of the module's own regulator; naming it `vcc` would tell the
    // solver the two pins are one node, which is exactly what the regulator
    // makes them not.
    assert_eq!(m.pins.get("24").map(String::as_str), Some("raw"));
    assert_ne!(m.pins.get("24").map(String::as_str), Some("vcc"));

    // Pin-compatible successors with DIFFERENT silicon must not bind: an
    // nRF52840 on the AVR core would be the wrong instruction set entirely.
    for value in ["Elite-C", "EliteC", "nice!nano", "nice_nano", "Pro Mini"] {
        assert!(
            resolve(value).is_none_or(|m| m.id != "pro_micro"),
            "{value} must not resolve to the Pro Micro entry"
        );
    }

    // The 3.3 V / 8 MHz spellings must NOT bind: the AVR backend runs every core
    // at 16 MHz and says nothing about it, so a board that names 8 MHz out loud is
    // telling us its firmware timing would be wrong by a factor of two. An honest
    // unresolved beats a silent 2x.
    for value in ["ProMicro 3V3", "Pro Micro 8MHz", "ProMicro-8MHz"] {
        assert!(
            resolve(value).is_none_or(|m| m.id != "pro_micro"),
            "{value} names a clock the AVR backend cannot honour and must not bind"
        );
    }
}

// ── 3. The part number that IS the output voltage ────────────────────────────

/// Three LDO families where the ordering code carries the rail. `bind_vreg`
/// stamps an ideal source at `vout`, so an entry that matched its whole family
/// would hold a 1.8 V net at 3.3 V and then report everything downstream as
/// over-volted. Each of these must bind ONLY its own voltage code.
#[test]
fn each_ldo_voltage_code_binds_only_its_own_rail() {
    for (value, id) in [
        ("TPS79733", "tps79733"),
        ("TPS79733DCKR", "tps79733"),
        ("TPS7A0533DBV", "tps7a0533"),
        ("TPS7A0533DBVR", "tps7a0533"),
        ("XC6204B33", "xc6204b33"),
        ("XC6204B332PR-G", "xc6204b33"),
    ] {
        let m = expect_id(value, id);
        assert_eq!(m.kind, ComponentKind::Vreg);
        assert_eq!(
            m.params.get_f64("vout"),
            Some(3.3),
            "{value} regulates 3.3 V"
        );
    }

    // The other voltage options of the same families. Each must resolve to
    // NOTHING (an honest gap) rather than to a 3.3 V entry.
    for value in [
        "TPS79718",     // 1.8 V
        "TPS79730",     // 3.0 V
        "TPS797285",    // 2.85 V
        "TPS7A0518DBV", // 1.8 V
        "TPS7A0550DBV", // 5.0 V
        "XC6204B18",    // 1.8 V
        "XC6204B50",    // 5.0 V
        "XC6205B33",    // the sub-1.8 V sibling family
    ] {
        let bound = resolve(value).map(|m| m.params.get_f64("vout"));
        assert!(
            bound != Some(Some(3.3)),
            "{value} must not bind at 3.3 V; it resolved to {bound:?}"
        );
    }
}

/// The XC6204's CE polarity is in the SAME ordering-code position as its current
/// rating, and getting it wrong inverts the rail: a C/D/G/H part is active-LOW,
/// so a board holding CE high would have its rail modelled present exactly when
/// the silicon switches it off. Only the B type may bind this entry.
#[test]
fn the_xc6204_ce_polarity_letter_gates_the_entry() {
    expect_id("XC6204B33", "xc6204b33");
    for value in [
        "XC6204A33", // active high, but with an internal CE pull-down
        "XC6204C33", // active LOW
        "XC6204D33", // active LOW
        "XC6204F33", // 300 mA, active high
        "XC6204G33", // 300 mA, active LOW
    ] {
        assert!(
            resolve(value).is_none_or(|m| m.id != "xc6204b33"),
            "{value} must not resolve to the XC6204B (150 mA, active-high) entry"
        );
    }
}

/// A series voltage reference is a regulator whose distinguishing feature is
/// accuracy, and accuracy is what this schema has no field for. Both references
/// off the external five carry their own rail and nothing else's.
#[test]
fn the_voltage_references_carry_their_own_rail() {
    let m = expect_id("MAX6070AAUT18", "max6070_1v8");
    assert_eq!(m.kind, ComponentKind::Vreg);
    assert_eq!(m.params.get_f64("vout"), Some(1.8));
    // 19-6355 Rev 18 Pin Description, MAX6070 column. OUTF is the pin the ideal
    // source is stamped on; OUTS is a sense leg that carries no device.
    assert_pins(
        &m,
        &[
            ("1", "filter"),
            ("2", "gnd"),
            ("3", "en"),
            ("4", "in"),
            ("5", "outs"),
            ("6", "out"),
        ],
    );
    // The MAX6071 is the same reference with a Kelvin GROUND pair where the
    // MAX6070 has GND and FILTER, so pads 1 and 2 mean different things on it.
    for value in ["MAX6071AAUT18", "MAX6070AAUT25", "MAX6070AAUT50"] {
        assert!(
            resolve(value).is_none_or(|m| m.id != "max6070_1v8"),
            "{value} must not resolve to the MAX6070 1.8 V entry"
        );
    }

    let m = expect_id("LT1461AxS8-3.3", "lt1461a_3v3");
    assert_eq!(m.params.get_f64("vout"), Some(3.3));
    for value in ["LT1461AIS8-3.3", "LT1461ACS8-3.3", "LT1461BIS8-3.3"] {
        expect_id(value, "lt1461a_3v3");
    }
    // SHDN is active LOW. Naming it `en` would invert its sense the moment
    // anything gates on the role.
    assert_eq!(m.pins.get("3").map(String::as_str), Some("shdn"));
    assert_eq!(m.pins.get("2").map(String::as_str), Some("in"));
    assert_eq!(m.pins.get("4").map(String::as_str), Some("gnd"));
    assert_eq!(m.pins.get("6").map(String::as_str), Some("out"));
    for value in ["LT1461AIS8-2.5", "LT1461AIS8-5", "LT1461AIS8-4.096"] {
        assert!(
            resolve(value).is_none_or(|m| m.id != "lt1461a_3v3"),
            "{value} must not resolve to the 3.3 V LT1461 entry"
        );
    }
}

// ── 4. The bare part against the graded bin ──────────────────────────────────

/// The library had a BC807-40 card and nothing for the bare BC807, so eight
/// transistors on an RF instrument were reported as open circuits while a card
/// for a different gain bin sat next to them. The bins are not interchangeable:
/// this family's hFE window runs 100..600 ungraded and 250..600 on the -40, and
/// Nexperia ships one SPICE card per bin.
#[test]
fn the_bare_bc807_gets_the_ungraded_card_and_not_the_minus_forty_bin() {
    let bare = expect_id("BC807", "bc807");
    assert_eq!(bare.kind, ComponentKind::BjtPnp);
    assert_eq!(
        bare.params.get_f64("bf"),
        Some(300.0),
        "the ungraded card's BF, for the 100..600 hFE window"
    );

    let graded = expect_id("BC807-40", "bc807_40");
    assert_eq!(
        graded.params.get_f64("bf"),
        Some(535.0),
        "the -40 card's BF, for the 250..600 window"
    );

    // Package and packing suffixes are the same die and share the ungraded card.
    for value in ["BC807W", "BC807,215", "BC807,235"] {
        expect_id(value, "bc807");
    }

    // The graded spellings must NOT fall through to the ungraded card: -16 and
    // -25 have their own published cards and resolve to nothing until those are
    // entered, which is the honest outcome rather than a borrowed beta.
    for value in ["BC807-16", "BC807-25"] {
        assert!(
            resolve(value).is_none_or(|m| m.id != "bc807"),
            "{value} is a different gain bin and must not take the ungraded card"
        );
    }

    // And the NPN complement must stay well clear: same numbering scheme, opposite
    // polarity, and a wrong-polarity transistor conducts backwards.
    for value in ["BC817", "BC817-40", "BC846", "BC847"] {
        assert!(
            resolve(value).is_none_or(|m| m.id != "bc807"),
            "{value} must not resolve to the PNP BC807 entry"
        );
    }
    assert_eq!(bare.kind, ComponentKind::BjtPnp);
}

// ── 5. The pinout that was wrong, on a part already in the library ───────────

/// The 74LVC1G66's map had pads 2 and 3 transposed, and every board carrying the
/// part paid for it.
///
/// This entry read pad 2 as GROUND and pad 3 as the second switch terminal. Both
/// vendors' datasheets say the opposite: pad 2 is a bidirectional signal and pad 3
/// is ground. A board that grounds both pads does not notice (the only board in
/// this tree fitting the part grounds both, on all 19 instances). A board that uses
/// pad 2 as a signal, which is what a BILATERAL switch is for, got a ~7 Ohm switch
/// stamped from pad 1 onto the GROUND PLANE and a live I/O read as ground, with no
/// warning, because every role the binder wanted was present. That is the failure
/// this whole file is about, sitting in the library rather than absent from it.
///
/// The premise of the earlier fix was also wrong and is worth recording so nobody
/// reinstates it: the two vendors were believed to DISAGREE about the pinout, which
/// would have made the bare JEDEC number unbindable. TI SCES323R and Nexperia
/// Rev. 14.1 agree pin for pin. There is one pinout, the bare number is
/// unambiguous, and the only real split is five-lead against six-lead.
#[test]
fn the_74lvc1g66_has_one_pinout_and_pad_three_is_ground() {
    // TI SCES323R Figures 4-1/4-2/4-3 and Nexperia Rev. 14.1 Table 3 give the SAME
    // five-lead order, so the bare JEDEC number and both vendors' spellings share
    // one entry.
    for value in [
        "74LVC1G66",
        "SN74LVC1G66",
        "SN74LVC1G66DBVR",
        "SN74LVC1G66DCKR",
        "74LVC1G66GW",
        "74LVC1G66GV",
        "74LVC1G66GW,125",
    ] {
        let m = expect_id(value, "lvc1g66");
        assert_eq!(m.kind, ComponentKind::AnalogSwitch);
    }
    let m = expect("74LVC1G66");

    // PAD 3 IS GROUND AND PAD 2 IS A SIGNAL. This entry's map used to have those
    // two transposed, which stamped a ~7 Ohm switch from pad 1 onto the ground
    // plane and read a live bidirectional I/O as ground, on every board carrying
    // the part. Both directions are asserted so neither can drift back.
    assert_pins(
        &m,
        &[
            ("1", "in_out_a"),
            ("2", "in_out_b"),
            ("3", "vss"),
            ("4", "ctrl"),
            ("5", "vcc"),
        ],
    );
    assert_ne!(
        m.pins.get("2").map(String::as_str),
        Some("vss"),
        "pad 2 is a switch terminal on every vendor's datasheet, not ground"
    );

    // An active-high enable must not be named `s0`, which the binder reads as the
    // control-LOW throw. (The consequence is asserted in the engine's own suite.)
    assert!(!m.pins.values().any(|r| r == "s0"));

    // The six-lead packages insert n.c. at pad 5 and move VCC to 6, so they get
    // their own entry rather than the five-lead map.
    for value in ["SN74LVC1G66DRYR", "SN74LVC1G66DSFR", "74LVC1G66GS"] {
        let m = expect_id(value, "lvc1g66_6pin");
        assert_pins(&m, &[("3", "vss"), ("5", "nc"), ("6", "vcc")]);
    }
}

// ── The rest of the list, held to the same bar ───────────────────────────────

/// Every remaining entry the external-five gate added, checked for the two things
/// that make an entry worth having: it resolves under the spelling the board
/// actually writes, and its pad map says what the datasheet says. The pads named
/// here are the ones whose mis-assignment would be electrically destructive
/// rather than merely wrong: supplies, grounds, and driven outputs.
#[test]
fn the_remaining_external_five_entries_resolve_with_the_right_supply_pads() {
    // FT231X: the QFN-20 and SSOP-20 have DIFFERENT pin orders, so only the Q
    // may borrow this map. VCCIO (pad 20) is the rail every logic level is
    // referenced to, not VCC (pad 12).
    let m = expect_id("FT231X-Q", "ft231x");
    assert_eq!(m.kind, ComponentKind::Digital);
    assert_pins(
        &m,
        &[
            ("3", "gnd"),
            ("12", "vcc"),
            ("17", "txd"),
            ("18", "dtr_n"),
            ("19", "rts_n"),
            ("20", "vccio"),
            ("EP", "gnd3"),
        ],
    );
    assert_eq!(m.params.get_str("supply_pin"), Some("12"));
    assert_eq!(m.params.get_str("gnd_pin"), Some("3"));
    assert!(
        resolve("FT231XS").is_none_or(|e| e.id != "ft231x"),
        "the SSOP-20 has a different pin order and must not borrow the QFN map"
    );

    // BME280: the LGA-8 connection diagram runs CLOCKWISE in top view, the
    // opposite of the usual convention, so this map cannot be re-derived by
    // inspecting a footprint. VDD is pad 8 and VDDIO pad 6.
    let m = expect_id("BME280", "bme280");
    assert_pins(
        &m,
        &[
            ("1", "gnd"),
            ("2", "csb"),
            ("3", "sda"),
            ("4", "scl"),
            ("5", "sdo"),
            ("6", "vddio"),
            ("7", "gnd2"),
            ("8", "vdd"),
        ],
    );
    for value in ["BMP280", "BME680"] {
        assert!(
            resolve(value).is_none_or(|e| e.id != "bme280"),
            "{value} is a different die with a different register map"
        );
    }

    // MCP3421: all eight address options share this pinout and these
    // electricals, and the address they differ in is not modelled here at all.
    let m = expect_id("MCP3421A1", "mcp3421");
    assert_eq!(m.kind, ComponentKind::Adc);
    assert_eq!(m.params.get_f64("bits"), Some(18.0));
    assert_pins(
        &m,
        &[
            ("1", "vin_plus"),
            ("2", "vss"),
            ("3", "scl"),
            ("4", "sda"),
            ("5", "vdd"),
            ("6", "vin_minus"),
        ],
    );
    for value in ["MCP3421A0", "MCP3421A7"] {
        expect_id(value, "mcp3421");
    }
    for value in ["MCP3422A0", "MCP3424A0"] {
        assert!(
            resolve(value).is_none_or(|e| e.id != "mcp3421"),
            "{value} is a multi-channel part in a larger package"
        );
    }

    // TPL5010: RSTn is OPEN DRAIN, so the entry must carry no `voh` for it to
    // borrow. The `y` prefix on WAKE is what makes the binder stamp a driver, and
    // its absence on RSTn is what stops one being invented.
    let m = expect_id("TPL5010DDC", "tpl5010");
    assert_pins(
        &m,
        &[
            ("1", "vdd"),
            ("2", "gnd"),
            ("3", "delay"),
            ("4", "done"),
            ("5", "wake"),
            ("6", "rst_n"),
        ],
    );
    assert!(
        resolve("TPL5110").is_none_or(|e| e.id != "tpl5010"),
        "the TPL5110 switches a load rail instead of asserting a reset"
    );

    // SN74AUP3G34: the three gates are NOT in order round the package. 1A's
    // output is pad 7, 2A's is pad 5, 3A's is pad 2, and a plausible-looking
    // in-order map would cross all three signals.
    let m = expect_id("SN74AUP3G34DCU", "sn74aup3g34");
    expect_id("SN74AUP3G34DQER", "sn74aup3g34");
    for value in ["SN74AUP3G34RSE", "SN74AUP3G34YFP", "SN74AUP3G34"] {
        assert!(
            resolve(value).is_none_or(|e| e.id != "sn74aup3g34"),
            "{value} does not name the DCU/DQE pin order this map is"
        );
    }
    assert_pins(
        &m,
        &[
            ("1", "a1"),
            ("2", "y3"),
            ("3", "a2"),
            ("4", "gnd"),
            ("5", "y2"),
            ("6", "a3"),
            ("7", "y1"),
            ("8", "vcc"),
        ],
    );

    // SN75176A: A and B are a DIFFERENTIAL pair. Neither may be `y`-prefixed,
    // because that would stamp two independent single-ended push-pull drivers
    // where the silicon has one differential driver.
    let m = expect_id("SN75176AD", "sn75176a");
    assert_pins(
        &m,
        &[
            ("1", "r"),
            ("4", "d"),
            ("5", "gnd"),
            ("6", "bus_a"),
            ("7", "bus_b"),
            ("8", "vcc"),
        ],
    );
    // voh/vol are the RECEIVER output's, not the driver's. The datasheet gives both
    // and they are a volt apart: driver VOH 3.7 V typ on the bus pins, receiver
    // VOH 2.7 V min on pad 1, which is the only pin this entry describes.
    assert_eq!(m.params.get_f64("voh"), Some(2.7));
    assert_eq!(m.params.get_f64("vol"), Some(0.45));

    // TCA9535: sixteen port pins that are inputs or outputs depending on a
    // configuration register this entry does not model, so NONE of them may be
    // `y`-prefixed. INT is open drain and likewise carries no driver.
    let m = expect_id("TCA9535RTWR", "tca9535");
    expect_id("TCA9535RGER", "tca9535");
    // TI's TSSOP/SSOP maps put INT on pin 1, P00 on 4, GND on 12 and VCC on 24,
    // against this map's P00 on 1, GND on 9 and VCC on 21. A PW part bound here
    // would have its supply and ground on two port pins.
    for value in ["TCA9535PWR", "TCA9535DBR", "TCA9535"] {
        assert!(
            resolve(value).is_none_or(|e| e.id != "tca9535"),
            "{value} does not name the QFN pin order this map is"
        );
    }
    assert_pins(
        &m,
        &[
            ("9", "gnd"),
            ("19", "scl"),
            ("20", "sda"),
            ("21", "vcc"),
            ("22", "int_n"),
            ("25", "gnd2"),
        ],
    );
    assert_eq!(m.pins.len(), 25, "24 leads plus the exposed pad");
    for (pad, role) in &m.pins {
        assert!(
            !role.starts_with('y'),
            "pad {pad} is {role}: this part's pin directions come from a register \
             map that is not modelled, so nothing here may claim to drive"
        );
    }

    // TLV3691: push-pull, so both output levels are real driven levels, and the
    // hysteresis field must carry the datasheet's own VHYS row and not the input
    // offset voltage (which is a different quantity the sheet also publishes).
    let m = expect_id("TLV3691DPF", "tlv3691");
    assert_eq!(m.kind, ComponentKind::Comparator);
    assert_eq!(m.params.get_f64("hysteresis"), Some(0.017));
    assert_ne!(
        m.params.get_f64("hysteresis"),
        Some(0.003),
        "0.003 is VOS, the input offset; VHYS is 17 mV"
    );
    assert_pins(
        &m,
        &[
            ("1", "in_plus"),
            ("2", "vss"),
            ("3", "in_minus"),
            ("4", "out"),
            ("5", "nc"),
            ("6", "vcc"),
        ],
    );
    // out_hi is the rail minus the UPPER-rail drop (70 mV), out_lo the LOWER-rail
    // one (35 mV). The two differ by a factor of two and swapping them is the easy
    // mistake, so both are pinned.
    assert_eq!(m.params.get_f64("out_hi"), Some(3.230));
    assert_eq!(m.params.get_f64("out_lo"), Some(0.035));
    // The DCK package is a FIVE-pin SC70 with VCC on pin 5, where this map calls
    // pin 5 a no-connect and puts VCC on a pin 6 a DCK part does not have.
    for value in ["TLV3691IDCK", "TLV3691IDCKR", "TLV3691AIDCK"] {
        assert!(
            resolve(value).is_none_or(|e| e.id != "tlv3691"),
            "{value} is the 5-pin DCK package and must not take the 6-pin map"
        );
    }

    // TSV914: a QUAD, with EVERY channel suffixed, A included. `bind_opamp` stamps
    // one device per complete `_a`.._d` channel and never reaches its bare-name
    // fallback once it has stamped any, so a quad whose A used bare roles would lose
    // exactly that channel. The supplies sit mid-side on a SOIC-14, not the corners.
    let m = expect_id("TSV914", "tsv914");
    assert_eq!(m.kind, ComponentKind::Opamp);
    assert_pins(
        &m,
        &[
            ("1", "out_a"),
            ("2", "in_minus_a"),
            ("3", "in_plus_a"),
            ("4", "vcc"),
            ("11", "vss"),
            ("14", "out_d"),
        ],
    );
    assert_eq!(m.pins.len(), 14);
    // Channel A MUST be suffixed. `bind_opamp` looks for out_a..out_d, stamps every
    // complete channel and returns as soon as it has stamped any; only if it finds
    // none does it fall back to the bare names. A quad whose A used bare roles and
    // whose B/C/D used suffixed ones would stamp exactly three amplifiers and
    // silently drop A, which is the reverse of what the map looks like it says.
    for sfx in ["_a", "_b", "_c", "_d"] {
        for base in ["out", "in_plus", "in_minus"] {
            let role = format!("{base}{sfx}");
            assert!(
                m.pins.values().any(|r| *r == role),
                "TSV914 must name {role}, or bind_opamp drops that channel"
            );
        }
    }
    assert!(
        !m.pins.values().any(|r| r == "out"),
        "a bare `out` on a quad is the role that gets dropped"
    );
    for value in ["TSV911", "TSV912"] {
        assert!(
            resolve(value).is_none_or(|e| e.id != "tsv914"),
            "{value} shares the die but not the pinout"
        );
    }

    // The three load switches. Each is bound through the analog-switch path, so
    // `com` and `s0` are the two terminals that decide where current can flow.
    let m = expect_id("TPS22916YFP", "tps22916");
    assert_eq!(m.kind, ComponentKind::AnalogSwitch);
    assert_pins(
        &m,
        &[
            ("A1", "in_out_a"),
            ("A2", "in_out_b"),
            ("B1", "vss"),
            ("B2", "ctrl"),
        ],
    );
    // The ACTIVE-LOW ordering options must not bind: no single `vth` serves both
    // senses, and an L part bound here would conduct on the half of the control
    // range where it is off.
    for value in ["TPS22916BL", "TPS22916CL", "TPS22916CNL", "TPS22916CNLYFPR"] {
        assert!(
            resolve(value).is_none_or(|e| e.id != "tps22916"),
            "{value} is the active-low variant and must not take an active-high model"
        );
    }

    let m = expect_id("MIC94090C6", "mic94090");
    assert_pins(
        &m,
        &[
            ("1", "in_out_a"),
            ("2", "vss"),
            ("3", "nc"),
            ("4", "in_out_b"),
            ("5", "gnd"),
            ("6", "ctrl"),
        ],
    );
    for value in ["MIC94091C6", "MIC94093C6", "MIC94090MT"] {
        assert!(
            resolve(value).is_none_or(|e| e.id != "mic94090"),
            "{value} adds a discharge FET, changes the ramp, or is the 4-lead UDFN"
        );
    }

    // TPS2104: the mux whose modelled throw MUST be IN2, because the
    // single-throw binder path closes on a HIGH control and IN2 is the throw the
    // real part selects when EN is high. `s0` on IN1 would conduct from the
    // battery exactly when the board has switched to USB.
    let m = expect_id("TPS2104", "tps2104");
    assert_pins(
        &m,
        &[
            ("1", "ctrl"),
            ("2", "vss"),
            ("3", "in_out_b"),
            ("4", "in_out_a"),
            ("5", "in1"),
        ],
    );
    assert_eq!(
        m.params.get_f64("ron"),
        Some(1.3),
        "the IN2 (PMOS) path's on-resistance, since that is the throw stamped; \
         the 250 mOhm figure belongs to the unmodelled IN1 path"
    );
    assert!(
        resolve("TPS2105").is_none_or(|e| e.id != "tps2104"),
        "the TPS2105 has the opposite enable polarity and would conduct on the \
         wrong half of the control range"
    );

    // NONE of the three active-high switches may name a terminal `s0`.
    // `bind_analog_switch`'s single-throw path senses the INVERTED control when the
    // second terminal it picks is the `s0` role, because `s0` is the SPDT
    // normally-closed throw. On an active-high load switch that models the rail off
    // when the silicon is on, undisclosed. The role names are the polarity here.
    for value in ["TPS22916YFP", "MIC94090C6", "TPS2104"] {
        let m = expect(value);
        assert!(
            !m.pins.values().any(|r| r == "s0"),
            "{value} names a pad `s0`, which inverts its enable in the binder"
        );
        assert!(
            m.pins.values().any(|r| r == "in_out_a") && m.pins.values().any(|r| r == "in_out_b"),
            "{value} must name its two terminals in_out_a / in_out_b"
        );
    }

    // The two discrete FETs, whose pad map is the only thing standing between a
    // conducting switch and an open circuit on an Eagle board.
    for (value, id, kind) in [
        ("DMG3404", "dmg3404l", ComponentKind::Nmos),
        ("DMG3404L", "dmg3404l", ComponentKind::Nmos),
        ("FDN360P", "fdn360p", ComponentKind::Pmos),
    ] {
        let m = expect_id(value, id);
        assert_eq!(m.kind, kind, "{value} polarity");
        assert_pins(&m, &[("1", "gate"), ("2", "source"), ("3", "drain")]);

        // THE THRESHOLD SIGN, which is the difference between a switch and a
        // short. `bind_mosfet` folds `vto` by the polarity sign, so a P-channel
        // entry that states a positive threshold reaches the solver as a
        // DEPLETION device: one that conducts with its gate at its source. On a
        // high-side load switch that is a rail permanently on, reported as
        // modelled. So a P-channel vto must be negative and an N-channel one
        // positive. The whole-library version of this check is
        // `every_mosfet_states_its_threshold_with_the_sign_its_polarity_needs`
        // below, because the trap is the schema's and not these parts'.
        let vto = m
            .params
            .get_f64("vto")
            .expect("a MOSFET states its threshold");
        match kind {
            ComponentKind::Pmos => assert!(
                vto < 0.0,
                "{value}: a P-channel vto must be negative, got {vto}"
            ),
            _ => assert!(
                vto > 0.0,
                "{value}: an N-channel enhancement vto must be positive, got {vto}"
            ),
        }
    }
    // The FDN306P is a different part (-12 V, -2.6 A) whose datasheet is widely
    // mislabelled as the FDN360P's.
    assert!(
        resolve("FDN306P").is_none_or(|e| e.id != "fdn360p"),
        "the FDN306P is a lower-voltage part, not this die"
    );
}

/// A board built around an MCU no emulator models must still bind
/// STRUCTURALLY, and it must be impossible for that binding to run firmware on
/// the wrong instruction set. The `none:` token is what buys both: the scheduler
/// refuses it loudly and by name, where a missing backend would fall through to
/// the AVR default and run ARM code on an 8-bit core.
#[test]
fn an_mcu_with_no_cosim_platform_binds_but_cannot_run_firmware() {
    let m = expect_id("EFM32PG22C200F512IM40", "efm32pg22");
    assert_eq!(m.kind, ComponentKind::Mcu);
    let backend = m.params.get_str("backend").expect("backend present");
    assert!(
        backend.starts_with("none:"),
        "backend is {backend:?}: an unmodelled family must say so explicitly, \
         never leave the field empty for the AVR default to claim"
    );
    assert!(
        !backend.contains("simavr"),
        "an ARM Cortex-M33 must never carry an AVR backend"
    );

    // Table 6.2, QFN40: the four supply domains are named separately because they
    // are separately bounded, and IOVDD is the one every GPIO threshold is a
    // ratio of.
    assert_pins(
        &m,
        &[
            ("1", "pc00"),
            ("9", "hfxtal_i"),
            ("10", "hfxtal_o"),
            ("11", "reset"),
            ("12", "dvdd"),
            ("13", "vss"),
            ("14", "nc"),
            ("21", "pa00"),
            ("30", "decouple"),
            ("32", "vregvdd"),
            ("35", "avdd"),
            ("36", "iovdd"),
            ("40", "pd00"),
            ("41", "vss4"),
        ],
    );
    assert_eq!(m.pins.len(), 41, "40 leads plus the exposed pad");

    // 3.8 V, not the 6 V of every AVR in the same file. A board hanging this part
    // off a 5 V rail is destroying it, and the stress watch can only say so if
    // this number is here.
    assert_eq!(m.ratings.max_voltage_v, Some(3.8));
    assert_eq!(m.ratings.max_pin_current_a, Some(0.05));

    // The 32-pin QFN and the other package options have a different allocation,
    // so the M40 code is required.
    for value in [
        "EFM32PG22C200F512IM32",
        "EFM32PG22",
        "EFM32PG23B200F512IM40",
    ] {
        assert!(
            resolve(value).is_none_or(|e| e.id != "efm32pg22"),
            "{value} must not borrow the QFN40 pin allocation"
        );
    }
}

/// The P-channel threshold sign, swept over the WHOLE model database rather than
/// the entries this file added.
///
/// `bind_mosfet` folds `vto` by the polarity sign on its way to the solver, so the
/// database's convention is the SPICE device one: negative for an enhancement
/// P-channel part, positive for an enhancement N-channel one. A P-channel entry
/// that states a positive threshold does not fail to bind and does not warn; it
/// binds as a DEPLETION device, one that conducts with its gate at its source. On
/// a high-side load switch, which is what most P-channel parts on a board are, the
/// result is a rail that reads as permanently present, reported as modelled.
///
/// Nothing in the schema catches this, `hauksbee models lint` accepts either sign
/// (both are inside the -10 to +10 V physical bound), and no per-entry test would
/// find it in an entry nobody thought to write one for. So this walks the TOML.
#[test]
fn every_mosfet_states_its_threshold_with_the_sign_its_polarity_needs() {
    #[derive(serde::Deserialize)]
    struct DbFile {
        #[serde(default)]
        models: Vec<hauksbee_models::ModelEntry>,
    }

    let db = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("db");
    let mut checked = 0usize;
    let mut failures: Vec<String> = Vec::new();

    let mut files: Vec<_> = std::fs::read_dir(&db)
        .expect("the db directory is part of the crate")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "toml"))
        .collect();
    files.sort();
    assert!(
        !files.is_empty(),
        "no db/*.toml found under {}",
        db.display()
    );

    for path in files {
        let text = std::fs::read_to_string(&path).expect("readable db file");
        // pin_rules.toml and load_profiles.toml hold other shapes; a file with no
        // `[[models]]` array simply contributes nothing here.
        let Ok(parsed) = toml::from_str::<DbFile>(&text) else {
            continue;
        };
        for m in parsed.models {
            let polarity_is_p = match m.kind {
                ComponentKind::Pmos => true,
                ComponentKind::Nmos => false,
                _ => continue,
            };
            let Some(vto) = m.params.get_f64("vto") else {
                failures.push(format!(
                    "{}: {} states no vto; a MOSFET without a threshold binds at \
                     the solver's default, which is nobody's part",
                    path.file_name().unwrap().to_string_lossy(),
                    m.id
                ));
                continue;
            };
            checked += 1;
            let ok = if polarity_is_p { vto < 0.0 } else { vto > 0.0 };
            if !ok {
                failures.push(format!(
                    "{}: {} is {:?} with vto = {vto}. The binder folds vto by the \
                     polarity sign, so this reaches the solver as {} V and models a \
                     DEPLETION device that conducts with its gate at its source.",
                    path.file_name().unwrap().to_string_lossy(),
                    m.id,
                    m.kind,
                    if polarity_is_p { -vto } else { vto },
                ));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "MOSFET threshold signs:\n    {}",
        failures.join("\n    ")
    );
    // A sweep that walked nothing is the vacuous pass this project refuses to
    // emit for boards, and it is no better here.
    assert!(
        checked >= 15,
        "only {checked} MOSFET entries were swept; the database has more than that, \
         so the walk is not seeing the files"
    );
}

/// A `y`-prefixed output role without a `[models.logic]` block is a fabricated
/// constant LOW, swept over the whole model database.
///
/// `digital::output_roles` answers "which pins get a Thevenin driver" from the
/// entry's `logic.outputs` when it has a logic block, and otherwise from the `y*`
/// role-name convention. `PinDriver::stamp` creates that driver ENABLED at 0 V,
/// and something has to compute a value for it afterwards. For a declarative part
/// the logic block does; for a part with no logic block NOTHING does, so the
/// driver stays at 0 V and enabled, and the board's net is held hard low through
/// `ron`.
///
/// That is worse than the unmodelled pin it was meant to represent, and it is
/// invisible: the part binds, the report says Digital, and a UART TX line or an
/// RS-485 receiver output silently becomes a short to ground. Three of the entries
/// added alongside this test had exactly that (an FT231X's TXD, an SN75176A's
/// receiver output and a TPL5010's WAKE), which is why the invariant is swept
/// rather than spot-checked: the naming convention reads like documentation and
/// behaves like a device.
#[test]
fn no_entry_names_a_driven_output_it_has_no_logic_to_drive() {
    #[derive(serde::Deserialize)]
    struct DbFile {
        #[serde(default)]
        models: Vec<hauksbee_models::ModelEntry>,
    }

    let db = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("db");
    let mut with_logic = 0usize;
    let mut failures: Vec<String> = Vec::new();

    let mut files: Vec<_> = std::fs::read_dir(&db)
        .expect("the db directory is part of the crate")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "toml"))
        .collect();
    files.sort();
    assert!(
        !files.is_empty(),
        "no db/*.toml found under {}",
        db.display()
    );

    for path in files {
        let text = std::fs::read_to_string(&path).expect("readable db file");
        let Ok(parsed) = toml::from_str::<DbFile>(&text) else {
            continue;
        };
        for m in parsed.models {
            let y_roles: Vec<&String> = m.pins.values().filter(|r| r.starts_with('y')).collect();
            if y_roles.is_empty() {
                continue;
            }
            if m.logic.is_empty() {
                failures.push(format!(
                    "{}: {} names {y_roles:?} but has no [models.logic] block, so \
                     nothing computes a value for the driver those roles stamp: \
                     each of those nets is held at 0 V through the driver's ron",
                    path.file_name().unwrap().to_string_lossy(),
                    m.id
                ));
            } else {
                with_logic += 1;
                // And where there IS a logic block, every `y` pin must be one of
                // its declared outputs, or the role name promises a driver the
                // logic never assigns to.
                for role in y_roles {
                    assert!(
                        m.logic.outputs.iter().any(|o| o == role),
                        "{}: {} names pin role {role} but its [models.logic] \
                         outputs are {:?}",
                        path.file_name().unwrap().to_string_lossy(),
                        m.id,
                        m.logic.outputs
                    );
                }
            }
        }
    }

    assert!(
        failures.is_empty(),
        "driven-output roles with nothing to drive them:\n    {}",
        failures.join("\n    ")
    );
    // The sweep must actually have seen the parts that legitimately use `y` roles,
    // or it is passing because it walked nothing.
    assert!(
        with_logic >= 5,
        "only {with_logic} entries with both `y` roles and a logic block were \
         swept; the database has more than that"
    );
}

/// The three parts this batch could NOT model each name their own unlocking input,
/// and the table that carries those sentences cannot make any of them bind.
///
/// The project's rule is that an abstention must name what would unlock it. Before
/// `db/unmodelled.toml` the report had one sentence for every gap ("No model in the
/// library matched this part") and one next step ("add a model to your models
/// directory"), which are also exactly what a part nobody has ever examined prints.
/// A reader could not tell "unexamined" from "examined, and here is the blocker".
///
/// Two things are asserted, and the second matters more than the first. The
/// sentences exist and are specific; AND every part named in that table still
/// resolves to NO MODEL, because a table that could quietly promote a part to
/// "covered" would be the exact dishonesty the abstention text is there to avoid.
#[test]
fn each_abstention_names_its_unlocking_input_and_binds_nothing() {
    let lib = ModelLibrary::builtin();
    let table = lib.unmodelled();
    assert!(
        !table.is_empty(),
        "the built-in unmodelled.toml must load; an empty table means the include \
         or the loader broke and every gap silently went back to the generic text"
    );

    for (value, must_mention) in [
        // The strap that decides the output format is the input; naming LVDS alone
        // would not be enough, so the sentence has to reach the strap pins.
        ("Si53301", &["SFOUT", "strap"][..]),
        // A device kind, not a document: the datasheet is complete and the schema
        // is what cannot express a differential output.
        ("LMX2572", &["differential"][..]),
        // The package contradiction is the finding here, so the disclosure has to
        // carry it rather than only asking for a SPICE card.
        ("BFP181", &["SOT143", "SPICE"][..]),
    ] {
        let note = table
            .note_for(value, "")
            .unwrap_or_else(|| panic!("{value} must have a named abstention"));

        assert!(
            note.because.len() > 80,
            "{value}: `because` is {} chars, which is too short to have said \
             anything specific",
            note.because.len()
        );
        assert!(
            note.unlocked_by.len() > 60,
            "{value}: `unlocked_by` is {} chars; an unlocking input has to be \
             specific enough to act on",
            note.unlocked_by.len()
        );
        for token in must_mention {
            let both = format!("{} {}", note.because, note.unlocked_by);
            assert!(
                both.contains(token),
                "{value}: the disclosure never mentions {token:?}, so it does not \
                 name the actual blocker"
            );
        }
        // "More work" is not an unlocking input.
        for vague in ["TODO", "not supported", "unsupported", "someday"] {
            assert!(
                !note.unlocked_by.to_lowercase().contains(vague),
                "{value}: `unlocked_by` contains the non-answer {vague:?}"
            );
        }

        // THE CONTAINMENT. An abstention must never resolve a part.
        assert!(
            resolve(value).is_none(),
            "{value} has a named abstention AND a model, which means the abstention \
             text is unreachable dead weight or, worse, the part is being counted as \
             covered while the report explains why it cannot be"
        );
    }
}
