use super::*;
use crate::schema::DbFile;

fn card(text: &str) -> ModelEntry {
    let mut db: DbFile = toml::from_str(text).expect("card parses");
    db.models.remove(0)
}

const LOAD_SWITCH: &str = r#"
[[models]]
id = "xsw1"
kind = "analog_switch"
description = "XSW1 high-side load switch"
[models.params]
en_on_level = "high"
[models.pins]
"1" = "in_out_b"
"2" = "vss"
"3" = "ctrl"
"4" = "in_out_a"
"#;

const SWITCH_CAPS: &str = r#"
[[models.supply_cap]]
role = "in_out_b"
side = "input"
grade = "recommended"
min_f = 1.0e-6
basis = "section 10 (PDF p.19)"
[[models.supply_cap]]
role = "in_out_a"
side = "output"
min_f = 1.0e-7
basis = "section 10 (PDF p.19)"
"#;

const MOTOR: &str = r#"
[[models]]
id = "xdrv"
kind = "digital"
description = "XDRV dual H-bridge motor driver"
[models.params]
identity_only = true
[models.pins]
"1" = "nsleep"
"2" = "aout1"
"3" = "vm"
"4" = "gnd"
"5" = "en_n"
"#;

const MCU: &str = r#"
[[models]]
id = "xmcu"
kind = "mcu"
description = "XMCU Cortex-M0 MCU with a ROM bootloader on USART1"
[models.pins]
"1" = "vdd"
"2" = "pa10"
"3" = "osc_in"
"4" = "osc_out"
"#;

fn has(found: &[String], word: &str) -> usize {
    found.iter().filter(|w| w.contains(word)).count()
}

/// A load switch with neither its capacitors nor its open-enable behaviour is
/// asked for both; stating them silences the lint.
#[test]
fn a_load_switch_missing_its_caps_and_open_enable_is_asked_for_both() {
    let found = warnings(&card(LOAD_SWITCH));
    assert_eq!(has(&found, "no [[models.supply_cap]]"), 1, "{found:?}");
    assert_eq!(has(&found, "states no open-pin behaviour"), 1, "{found:?}");

    let with_caps = warnings(&card(&format!("{LOAD_SWITCH}{SWITCH_CAPS}")));
    assert_eq!(has(&with_caps, "supply_cap"), 0, "{with_caps:?}");
    assert_eq!(has(&with_caps, "open-pin"), 1, "{with_caps:?}");

    let open = LOAD_SWITCH.replace(
        "en_on_level = \"high\"",
        "en_on_level = \"high\"\nen_open = \"off\"\nen_open_basis = \"p.3\"",
    );
    assert!(warnings(&card(&format!("{open}{SWITCH_CAPS}"))).is_empty());

    // An internal pull states the open pin just as well.
    let pull = LOAD_SWITCH.replace(
        "en_on_level = \"high\"",
        "en_on_level = \"high\"\nen_pull_down_ohms = 7.5e5\nen_open_basis = \"p.1\"",
    );
    assert!(warnings(&card(&format!("{pull}{SWITCH_CAPS}"))).is_empty());
}

/// The honest exit: an `unknown` record naming the field satisfies the lint.
#[test]
fn an_unknown_record_is_the_honest_exit() {
    let record = |p: &str| {
        format!(
            "[models.source]\ntier = \"datasheet-derived\"\nvalidation = \"physical-bounds-only\"\n\
             [[models.source.uncertainty]]\nstatus = \"unknown\"\nparameter = \"{p}\"\n\
             reason = \"the datasheet prints no such row\"\n"
        )
    };
    let text = format!("{LOAD_SWITCH}{}", record("supply_cap"));
    let found = warnings(&card(&text));
    assert_eq!(has(&found, "supply_cap"), 0, "{found:?}");
    assert_eq!(has(&found, "open-pin"), 1, "{found:?}");
    let text = format!("{LOAD_SWITCH}{}", record("en_open"));
    let found = warnings(&card(&text));
    assert_eq!(has(&found, "open-pin"), 0, "{found:?}");
}

/// A signal switch, an LED driver and a plain regulator are no subject.
#[test]
fn parts_that_are_not_load_switches_or_motor_drivers_are_silent() {
    let signal = r#"
[[models]]
id = "mux"
kind = "analog_switch"
description = "dual SPDT analog multiplexer"
[models.pins]
"1" = "com"
"2" = "s0"
"3" = "s1"
"4" = "ctrl"
"#;
    assert!(warnings(&card(signal)).is_empty());
    let led = MOTOR
        .replace("dual H-bridge motor driver", "constant-current LED driver")
        .replace("\"vm\"", "\"vcc\"")
        .replace("aout1", "led1");
    assert!(
        warnings(&card(&led)).is_empty(),
        "{:?}",
        warnings(&card(&led))
    );
    let ldo = r#"
[[models]]
id = "ldo"
kind = "vreg"
description = "3.3 V LDO"
[models.pins]
"1" = "in"
"2" = "gnd"
"3" = "out"
"4" = "en"
"#;
    assert!(warnings(&card(ldo)).is_empty());
}

#[test]
fn a_motor_driver_is_asked_for_pin_roles_caps_enable_and_a_uart_bias() {
    // Roles present, nothing else.
    let found = warnings(&card(MOTOR));
    assert_eq!(has(&found, "no [[models.supply_cap]]"), 1, "{found:?}");
    assert_eq!(has(&found, "states no open-pin behaviour"), 1, "{found:?}");
    assert_eq!(has(&found, "role map"), 0, "{found:?}");

    // No pin roles at all: the pinout lived in notes.
    let bare = MOTOR
        .split("[models.pins]")
        .next()
        .unwrap_or_default()
        .to_owned();
    let found = warnings(&card(&bare));
    assert_eq!(
        has(&found, "no usable [models.pins] role map"),
        1,
        "{found:?}"
    );

    // A pin map without the motor supply is no role map either.
    let no_vm = MOTOR.replace("\"vm\"", "\"vcc\"");
    assert_eq!(
        has(&warnings(&card(&no_vm)), "role map"),
        1,
        "{:?}",
        warnings(&card(&no_vm))
    );

    // A single-wire UART pin with no bias row.
    let uart = format!("{MOTOR}\"6\" = \"pdn_uart\"\n");
    let found = warnings(&card(&uart));
    assert_eq!(has(&found, "single-wire UART pin"), 1, "{found:?}");
    let row = format!(
        "{uart}[[models.pin_bias]]\nrole = \"pdn_uart\"\nlevel = \"high\"\nneeds = \"pull\"\nbasis = \"p.9\"\n"
    );
    assert_eq!(has(&warnings(&card(&row)), "single-wire"), 0);

    // Everything stated: silent.
    let full = MOTOR.replace(
        "identity_only = true",
        "identity_only = true\nen_open = \"on\"\nen_open_basis = \"p.4\"",
    );
    let full = format!(
        "{full}[[models.supply_cap]]\nrole = \"vm\"\nside = \"input\"\nmin_f = 1.0e-4\nbasis = \"p.5\"\n"
    );
    assert!(
        warnings(&card(&full)).is_empty(),
        "{:?}",
        warnings(&card(&full))
    );
}

/// A motor driver without an enable role is not asked what an open enable does.
#[test]
fn a_motor_driver_with_no_enable_pin_is_not_asked_about_one() {
    let no_en = MOTOR.replace("\"5\" = \"en_n\"\n", "");
    let found = warnings(&card(&no_en));
    assert_eq!(has(&found, "open-pin"), 0, "{found:?}");
    assert_eq!(has(&found, "supply_cap"), 1, "{found:?}");
}

#[test]
fn coverage_naming_an_open_drain_output_needs_an_open_drain_pad() {
    let monitor = |extra: &str| {
        format!(
            "[[models]]\nid = \"mon\"\nkind = \"digital\"\ndescription = \"I2C power monitor\"\n\
             [models.coverage]\nimplements = [\"pin_roles\", \"open_drain_low_level\"]\n\
             [models.pins]\n\"1\" = \"vcc\"\n\"2\" = \"gnd\"\n\"3\" = \"alert\"\n{extra}"
        )
    };
    let found = warnings(&card(&monitor("")));
    assert_eq!(has(&found, "open-drain characteristic"), 1, "{found:?}");
    let pads = "[models.pin_capability.open_drain]\nbasis = \"p.4 pin table\"\n\
                [models.pin_capability.open_drain.pads]\n\"3\" = \"ALERT\"\n";
    assert!(warnings(&card(&monitor(pads))).is_empty());
    // A table with no pad is no table.
    let empty = "[models.pin_capability.open_drain]\nbasis = \"p.4\"\n";
    assert_eq!(
        has(
            &warnings(&card(&monitor(empty))),
            "open-drain characteristic"
        ),
        1
    );
    // Saying the part is open-drain in `missing` still needs the pads.
    let missing = monitor("").replace("implements", "missing");
    assert_eq!(has(&warnings(&card(&missing)), "open-drain"), 1);
    // No claim, no warning.
    let quiet = monitor("").replace("open_drain_low_level", "alert_output");
    assert!(warnings(&card(&quiet)).is_empty());
}

#[test]
fn an_mcu_with_a_serial_rom_bootloader_is_asked_for_bias_rows() {
    let found = warnings(&card(MCU));
    assert_eq!(
        has(&found, "ROM bootloader but no [[models.pin_bias]]"),
        1,
        "{found:?}"
    );
    let row = format!(
        "{MCU}[[models.pin_bias]]\nrole = \"pa10\"\nlevel = \"high\"\nneeds = \"idle\"\n\
         opposed_only = true\nbasis = \"AN2606 Table 45, PDF p.116\"\n"
    );
    let found = warnings(&card(&row));
    assert_eq!(has(&found, "pin_bias"), 0, "{found:?}");
    // A USB-only boot ROM listens on no UART pin.
    let usb = format!("{MCU}[models.rom_bootloader]\nprotocol = \"usb_only\"\n")
        .replace(" with a ROM bootloader on USART1", "");
    assert_eq!(has(&warnings(&card(&usb)), "pin_bias"), 0);
    // A structured serial bootloader is enough, whatever the prose says.
    let structured = format!("{MCU}[models.rom_bootloader]\nprotocol = \"st_usart\"\n")
        .replace(" with a ROM bootloader on USART1", "");
    assert_eq!(has(&warnings(&card(&structured)), "pin_bias"), 1);
    // A part with no bootloader is not asked.
    let plain = MCU.replace(" with a ROM bootloader on USART1", "");
    assert_eq!(has(&warnings(&card(&plain)), "pin_bias"), 0);
}

#[test]
fn an_mcu_with_oscillator_pins_is_asked_for_the_load_cap_range() {
    let plain = MCU.replace(" with a ROM bootloader on USART1", "");
    let found = warnings(&card(&plain));
    assert_eq!(has(&found, "crystal load-capacitor range"), 1, "{found:?}");
    let ranged = plain.replace(
        "[models.pins]",
        "[models.params]\nxtal_cap_min_f = 1.2e-11\nxtal_cap_max_f = 2.2e-11\n[models.pins]",
    );
    assert_eq!(has(&warnings(&card(&ranged)), "crystal"), 0);
    let on_die = format!(
        "{plain}[[models.pin_capability.xtal_int_caps]]\noscillator = \"HFXO\"\npads = [\"3\", \"4\"]\nbasis = \"p.9\"\n"
    );
    assert_eq!(has(&warnings(&card(&on_die)), "crystal"), 0);
}

/// The prompt asks for each group on the kinds that carry it, and the checklist
/// helper lists the same groups.
#[test]
fn the_prompt_and_checklist_ask_for_each_group_on_its_kind() {
    let switch = prompt_guidance("analog_switch");
    for needle in [
        "[[models.supply_cap]]",
        "LOAD SWITCH",
        "en_open",
        "pin_capability.open_drain",
    ] {
        assert!(switch.contains(needle), "{needle}");
    }
    let digital = prompt_guidance("digital");
    for needle in [
        "MOTOR DRIVER",
        "[models.pins] MUST be",
        "pin_bias",
        "pdn_uart",
    ] {
        assert!(digital.contains(needle), "{needle}");
    }
    let mcu = prompt_guidance("mcu");
    for needle in [
        "ROM bootloader",
        "[[models.pin_bias]]",
        "xtal_cap_min_f",
        "xtal_int_caps",
    ] {
        assert!(mcu.contains(needle), "{needle}");
    }
    // A discrete part is asked for none of it.
    for kind in ["diode", "nmos", "bjt_npn", "passive", "connector"] {
        assert!(prompt_guidance(kind).is_empty(), "{kind}");
    }
    assert!(!prompt_guidance("vreg").contains("LOAD SWITCH"));
    assert!(!prompt_guidance("vreg").contains("MOTOR DRIVER"));

    let lines = checklist_lines(
        ComponentKind::AnalogSwitch,
        &["in_out_a", "in_out_b", "ctrl"],
    );
    assert!(lines.iter().any(|l| l.starts_with("supply_cap")));
    assert!(lines.iter().any(|l| l.starts_with("en_open")));
    let lines = checklist_lines(ComponentKind::Mcu, &["vdd"]);
    assert!(lines.iter().any(|l| l.starts_with("pin_bias")));
    assert!(lines.iter().any(|l| l.starts_with("xtal_cap")));
    assert!(checklist_lines(ComponentKind::Diode, &["anode", "cathode"]).is_empty());
}

/// The schema the prompt shows is the schema the validator and parser accept:
/// a load switch written exactly as the guidance describes parses, validates and
/// draws no field warning.
#[test]
fn a_card_written_as_the_prompt_asks_validates_and_is_silent() {
    let text = format!(
        "{LOAD_SWITCH}{SWITCH_CAPS}{}",
        r#"
[models.pin_capability.open_drain]
basis = "p.4 pin table: FAULT is open-drain"
[models.pin_capability.open_drain.pads]
"3" = "FAULT"
"#
    )
    .replace(
        "en_on_level = \"high\"",
        "en_on_level = \"high\"\nen_open = \"undefined\"\nen_open_basis = \"p.3: do not leave ON floating\"\nmust_not_float_roles = \"ctrl\"\nron = 0.07\nroff = 5.0e7\nvth = 1.0",
    );
    let entry = card(&text);
    assert!(warnings(&entry).is_empty(), "{:?}", warnings(&entry));
    crate::validation::validate(&entry).unwrap_or_else(|e| panic!("{e:?}"));
}

/// Replay of the shipped library: the lint's subjects are found, and the cards
/// the library itself calls load switches or motor drivers carry the fields the
/// prompt now asks for. Prints the cards that would draw a warning.
#[test]
fn shipped_load_switch_and_motor_driver_cards_are_the_lint_subjects() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("db");
    let (mut switches, mut motors, mut mcus) = (0, 0, 0);
    let mut noisy = Vec::new();
    for file in std::fs::read_dir(&dir).expect("db dir").flatten() {
        let path = file.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(db) = toml::from_str::<DbFile>(&text) else {
            continue;
        };
        for entry in &db.models {
            switches += usize::from(is_load_switch(entry));
            motors += usize::from(is_motor_driver(entry));
            mcus += usize::from(declares_serial_bootloader(entry));
            for w in warnings(entry) {
                noisy.push(format!(
                    "{}: {}",
                    entry.id,
                    w.chars().take(70).collect::<String>()
                ));
            }
        }
    }
    assert!(switches >= 5, "load switches found: {switches}");
    assert!(motors >= 3, "motor drivers found: {motors}");
    assert!(mcus >= 3, "serial-bootloader MCUs found: {mcus}");
    // The two cards the capacitor and enable work was done on are clean.
    for id in ["tps22916", "drv8833_pwp16"] {
        let hits: Vec<_> = noisy
            .iter()
            .filter(|n| n.starts_with(&format!("{id}:")))
            .collect();
        assert!(hits.is_empty(), "{hits:?}");
    }
    eprintln!("shipped cards drawing a field warning ({}):", noisy.len());
    for n in &noisy {
        eprintln!("  {n}");
    }
}
