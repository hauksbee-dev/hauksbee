//! `hauksbee-ci init <board>`: the generated starter spec must parse back through
//! the crate's own spec loader, and a second init must refuse to clobber it.
//!
//! The whole promise is "your first spec is an edit, not a blank page", which is
//! only true if what we emit is a spec the loader actually accepts. This binds a
//! real board (the committed AVR blinky, which has a detectable MCU and a +5V
//! rail), scaffolds a spec beside a temp copy, and loads it.

use std::path::PathBuf;

use hauksbee_ci::{init::init_to, run, Spec};

/// A board with a detectable MCU (atmega328p) and a named +5V rail.
use crate::support::blinky_board as blinky;
use crate::support::{example, repo, run_config};

/// The shipped Watchy board: a USB-fed VBUS rail plus a +3V3 rail the board
/// derives from it, so the scaffold has both a source rail and a derived one.
fn watchy() -> PathBuf {
    example("boards/watchy.kicad_pcb")
}

/// An STM32F103 blue pill board: its MCU binds to the external `renode:stm32f103`
/// backend, whose `SoC` descriptor maps every port's CRL/CRH direction register,
/// so it CAN report pin drive direction and boot-coverage scaffolds without the
/// backend-gap note.
fn stm32_bluepill() -> PathBuf {
    repo("testdata/boards/stm32_bluepill_demo.kicad_pcb")
}

/// An ESP32 devkit board: its MCU binds to the `qemu:esp32` backend, which
/// observes GPIO through a RAM mailbox carrying LEVELS only; it cannot report
/// pin drive direction, so boot-coverage keeps the honest backend-gap note.
fn esp32_devkit() -> PathBuf {
    repo("testdata/boards/esp32_devkit_demo.kicad_pcb")
}

/// Copy `src` (a `.kicad_pcb`) into a fresh per-test temp dir, returning the
/// copy's path, so init writes its `.toml` there rather than into the source
/// tree. The `tag` keeps parallel tests off each other's directory.
fn copy_board_to_tempdir(src: &std::path::Path, tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hauksbee_ci_init_{}_{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = src.file_name().unwrap();
    let dst = dir.join(file);
    std::fs::copy(src, &dst).unwrap();
    // A stale spec from a previous run would trip the overwrite guard.
    let stem = src.file_stem().unwrap().to_str().unwrap();
    let _ = std::fs::remove_file(dir.join(format!("{stem}.toml")));
    dst
}

#[test]
fn init_generates_a_spec_the_loader_accepts() {
    let board = copy_board_to_tempdir(&blinky(), "load");
    let spec_path = init_to(&board, board.parent()).expect("init scaffolds a spec");

    // It landed beside the board as <stem>.toml.
    assert_eq!(spec_path.file_name().unwrap(), "blinky.toml");
    assert!(
        spec_path.exists(),
        "the spec file should be written to disk"
    );

    // The generated spec round-trips through the crate's own loader (the point of
    // the feature). Structural validation runs here too, so a bad scaffold fails.
    let spec = Spec::load(&spec_path).expect("generated spec parses through Spec::load");

    // The scaffold reflects what the board actually is: the detected MCU and the
    // detected +5V supply leg.
    assert_eq!(
        spec.mcu_note(),
        Some("atmega328p"),
        "detected MCU is filled in"
    );
    assert!(
        spec.supplies.iter().any(|s| s.net == "+5V"),
        "the +5V supply leg the binder detected is scaffolded"
    );
    // The starter must run GREEN out of the box: only `no_faults` is live.
    // boot-coverage is scaffolded COMMENTED-OUT on every backend (even the AVR
    // in-process one), because it asserts on firmware behaviour and `firmware =`
    // is itself commented in the starter, left live it goes RED on the first run
    // (the exact false-red this scaffold used to emit). The rail voltage asserts also
    // stay commented. So exactly one assertion loads.
    let kinds: Vec<&str> = spec.asserts.iter().map(|a| a.kind.as_str()).collect();
    assert!(
        kinds.contains(&"no_faults"),
        "a no_faults assertion is enabled"
    );
    assert!(
        !kinds.contains(&"boot_coverage"),
        "boot-coverage is scaffolded commented-out so the starter is GREEN out of the box"
    );
    assert_eq!(
        spec.asserts.len(),
        1,
        "only the no_faults assertion is live"
    );

    // The rendered text still carries a (commented) boot-coverage block so the
    // user can opt in after wiring firmware.
    let text = hauksbee_ci::init::render_spec(&board).expect("render scaffolds a spec");
    for field in ["# bom =", "# bom_columns =", "# placement =", "# variant ="] {
        assert!(
            text.contains(field),
            "the starter must make assembly inputs discoverable without enabling them: {field}\n{text}"
        );
    }
    assert!(
        text.contains("# kind = \"boot_coverage\""),
        "a commented boot-coverage block is present to opt into, got:\n{text}"
    );
    assert!(
        !text.contains("\nkind = \"boot_coverage\""),
        "boot-coverage must not be a live assertion in the starter, got:\n{text}"
    );
}

#[test]
fn init_comments_out_boot_coverage_when_the_backend_cannot_satisfy_it() {
    // The ESP32 devkit binds to the `qemu:esp32` backend, whose GPIO mailbox
    // carries pin LEVELS only; it cannot report pin drive direction, so it
    // cannot tell a held-LOW control net from an undriven one (docs/cosim/MCU.md). A
    // live boot-coverage assertion there can go RED with a misleading diagnosis
    // on a net the firmware actually drives, so init must scaffold it
    // commented-out with an honest note rather than as a live assertion.
    let board = copy_board_to_tempdir(&esp32_devkit(), "backend_gap");

    // The rendered text carries the honest backend-gap note and a commented-out
    // (`# `) boot-coverage assertion, not a live one.
    let text = hauksbee_ci::init::render_spec(&board).expect("render scaffolds a spec");
    assert!(
        text.contains("qemu:esp32"),
        "the note names the backend that cannot satisfy the assertion, got:\n{text}"
    );
    assert!(
        text.contains("# kind = \"boot_coverage\""),
        "boot-coverage is scaffolded commented-out, got:\n{text}"
    );
    assert!(
        !text.contains("\nkind = \"boot_coverage\""),
        "boot-coverage must not be a live assertion on this backend, got:\n{text}"
    );

    // It still writes and round-trips through the loader, with the live-assertion
    // set reduced to `no_faults` only (boot-coverage did not load).
    let spec_path = init_to(&board, board.parent()).expect("init scaffolds a spec");
    let spec = Spec::load(&spec_path).expect("generated spec parses through Spec::load");
    let kinds: Vec<&str> = spec.asserts.iter().map(|a| a.kind.as_str()).collect();
    assert!(kinds.contains(&"no_faults"), "no_faults stays live");
    assert!(
        !kinds.contains(&"boot_coverage"),
        "boot-coverage is commented out, so it must not load"
    );
}

/// The capability flip: `renode:stm32f103` maps every port's direction
/// register (CRL/CRH) in its `SoC` descriptor, so it now REPORTS pin drive
/// direction and the scaffold treats it like the AVR backend; the same
/// commented-out starter block (GREEN out of the box, firmware is commented
/// too) but WITHOUT the direction-gap note that direction-blind backends get.
#[test]
fn init_omits_the_backend_gap_note_for_direction_mapped_renode_parts() {
    let board = copy_board_to_tempdir(&stm32_bluepill(), "dir_mapped");
    let text = hauksbee_ci::init::render_spec(&board).expect("render scaffolds a spec");
    assert!(
        !text.contains("cannot report pin drive DIRECTION"),
        "a dir-mapped Renode part must not carry the direction-gap note, got:\n{text}"
    );
    assert!(
        text.contains("# kind = \"boot_coverage\""),
        "boot-coverage still scaffolds commented-out (firmware is commented too), got:\n{text}"
    );
}

/// Uncomment every commented-out TOML block the scaffold emits, exactly as a
/// user following the file's own instructions would: strip the `# ` prefix from
/// table headers and `key = value` lines (prose comments stay comments), and
/// fill in the one value the scaffold explicitly asks for (an unpowered rail's
/// blank `volts =`). Returns the resulting spec text.
fn uncomment_scaffold(text: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        // Uncommented lines have nothing to strip: an empty `t` keeps them as-is.
        let t = line.strip_prefix("# ").map_or("", str::trim_start);
        let is_table = t.starts_with("[[") || (t.starts_with('[') && t.contains(']'));
        let (key, value) = t.split_once('=').unwrap_or_default();
        let key = key.trim();
        let is_kv = !key.is_empty()
            && key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.');
        // The unpowered-rail block deliberately leaves `volts =` blank and
        // tells the user to fill it in; do what the user would.
        let blank = value.split('#').next().unwrap_or("").trim().is_empty();
        let kept = if is_kv && blank {
            format!("{key} = 3.3")
        } else if is_table || is_kv {
            t.to_string()
        } else {
            line.to_string()
        };
        out.push_str(&kept);
        out.push('\n');
    }
    out
}

/// The regression the missing-`part` scenario block caused: a user who
/// uncomments exactly what init wrote must get a spec the loader accepts, on
/// every board class the scaffold branches for (AVR with supply + scenario,
/// direction-mapped Renode, direction-blind qemu).
#[test]
fn every_commented_scaffold_block_parses_when_uncommented() {
    for (src, tag) in [
        (blinky(), "uncomment_avr"),
        (stm32_bluepill(), "uncomment_stm32"),
        (esp32_devkit(), "uncomment_esp32"),
    ] {
        let board = copy_board_to_tempdir(&src, tag);
        let text = hauksbee_ci::init::render_spec(&board).expect("render scaffolds a spec");
        let uncommented = uncomment_scaffold(&text);
        let spec_path = board.with_extension("uncommented.toml");
        std::fs::write(&spec_path, &uncommented).unwrap();
        let spec = Spec::load(&spec_path).unwrap_or_else(|e| {
            panic!(
                "uncommenting every scaffolded block on {tag} must parse, got: {e}\n\
                 spec text:\n{uncommented}"
            )
        });
        // The scaffolded [[scenario]] carries its required `part` (the exact
        // field whose absence made the uncommented block fail to parse).
        if text.contains("# [[scenario]]") {
            assert!(
                text.contains("# part = \""),
                "{tag}: the commented scenario block must scaffold `part`, got:\n{text}"
            );
            assert!(
                spec.scenarios.iter().all(|s| !s.part.is_empty()),
                "{tag}: the uncommented scenario carries a part"
            );
        }
    }
}

/// `--out` resolution: the `.toml` suffix decides file-versus-directory, so a
/// destination that does not exist yet still reads as the directory the user
/// meant. The regression: `--out ci` on a repo with no `ci/` wrote a FILE named
/// `ci`, and the guidance printed underneath then told the user specs are
/// discovered in `ci/`, so the spec sat somewhere nothing reads and the gate
/// checked nothing without ever saying so.
#[test]
fn out_without_a_toml_suffix_is_a_directory_even_when_it_does_not_exist_yet() {
    let board = copy_board_to_tempdir(&blinky(), "out_shapes");
    let root = board.parent().unwrap().to_path_buf();

    // A bare name, no trailing slash, nothing on disk.
    let fresh = root.join("ci");
    let spec = init_to(&board, Some(&fresh)).expect("init scaffolds into a fresh directory");
    assert_eq!(spec, fresh.join("blinky.toml"));
    assert!(fresh.is_dir(), "the destination is created as a directory");
    assert!(spec.is_file(), "and the spec is the file inside it");
    Spec::load(&spec).expect("the spec inside the new directory loads");

    // A nested path, still no `.toml`, still a directory (parents and all).
    let nested = root.join("nested/deeper/checks");
    let spec =
        init_to(&board, Some(&nested)).expect("init scaffolds into a fresh nested directory");
    assert_eq!(spec, nested.join("blinky.toml"));
    assert!(nested.is_dir());

    // A `.toml` suffix is the spec file itself, with its parent created.
    let file = root.join("specs/power-up.toml");
    let spec = init_to(&board, Some(&file)).expect("init writes the named file");
    assert_eq!(spec, file);
    assert!(file.is_file(), "a .toml destination is the file, not a dir");

    // Case does not change the meaning of the suffix.
    let shouty = root.join("SHOUTY.TOML");
    let spec = init_to(&board, Some(&shouty)).expect("init writes the named file");
    assert_eq!(spec, shouty);
    assert!(shouty.is_file());
}

/// Uncomment the scaffolded blocks a user can turn on WITHOUT wiring firmware:
/// supplies, voltage asserts, and the `profile/scenario/rail_window` group. The
/// firmware-dependent blocks (the `firmware = ` line itself, `boot_coverage`,
/// uart, toggle) keep their comments, exactly as the scaffold's own notes
/// instruct. Paragraph-scoped, because that is the unit a user actually strips.
fn uncomment_board_blocks(text: &str) -> String {
    let needs_firmware = |p: &str| {
        ["firmware", "boot_coverage", "\"uart\"", "\"toggle\""]
            .iter()
            .any(|t| p.contains(t))
    };
    let mut out = String::new();
    for paragraph in text.split("\n\n") {
        if needs_firmware(paragraph) {
            out.push_str(paragraph);
        } else {
            out.push_str(&uncomment_scaffold(paragraph));
        }
        out.push_str("\n\n");
    }
    out
}

/// E46: the scaffold must not teach a hollow gate. A user who uncomments the
/// rail blocks the scaffold wrote, following its own instructions, gets a live,
/// falsifiable run and carries NO coverage-hole warning about the scaffold's
/// own output. A board with unresolved assumptions may now be INVALID rather
/// than GREEN; that is the evidence spine refusing to bless an untrustworthy
/// result. The original regression: the supply legs were scaffolded
/// `kind = "ideal"` and the voltage asserts pointed at those same nets, so the
/// very first run said the check it had just been handed cannot fail for a
/// board reason.
#[test]
fn uncommenting_the_scaffolded_rail_blocks_gates_for_real() {
    for (src, tag) in [(blinky(), "hollow_blinky"), (watchy(), "hollow_watchy")] {
        let board = copy_board_to_tempdir(&src, tag);
        let text = hauksbee_ci::init::render_spec(&board).expect("render scaffolds a spec");
        let spec_path = board.with_extension("rails.toml");
        let spec_text = uncomment_board_blocks(&text);
        std::fs::write(&spec_path, &spec_text).unwrap();

        // The rail check is really live, otherwise this test would pass on a
        // spec that asserts nothing.
        let spec = Spec::load(&spec_path)
            .unwrap_or_else(|e| panic!("{tag}: the uncommented rail spec loads, got: {e}"));
        assert!(
            spec.asserts.iter().any(|a| a.kind == "voltage"),
            "{tag}: a voltage assertion is live, got kinds {:?}",
            spec.asserts
                .iter()
                .map(|a| a.kind.as_str())
                .collect::<Vec<_>>()
        );
        assert!(
            !spec.supplies.is_empty(),
            "{tag}: the scaffolded supply leg is live"
        );

        let result = run(&run_config(spec_path.clone()))
            .unwrap_or_else(|e| panic!("{tag}: the uncommented rail spec runs, got: {e}"));
        let human = result.render_human();
        let undermined = result
            .evidence
            .iter()
            .any(hauksbee_ir::evidence::EvidenceMap::is_undermined);
        assert_eq!(
            result.exit_code(),
            if undermined { 3 } else { 0 },
            "{tag}: the scaffold is GREEN only with trustworthy evidence and INVALID when an \
             assertion is undermined, got:\n{human}"
        );
        assert!(
            !result
                .coverage_warnings
                .iter()
                .any(|w| w.contains("cannot fail for a board reason")),
            "{tag}: the scaffold's own rail checks must be falsifiable, got {:?}\nspec:\n{spec_text}",
            result.coverage_warnings
        );
        // No co-sim coverage hole: the scaffold must actually exercise what it
        // asserts. Model-provenance holes are a different class and are expected
        // here (the hollow_watchy fixture's MBR0530 diodes bind to the generic
        // signal-diode fallback), so they are excluded rather than silencing the
        // whole channel. So is a powered part with no model at all (watchy's M1
        // motor, between +3V3 and its driver), which the scheduler names itself.
        let model_gap = |w: &&String| {
            w.starts_with("models:") || w.contains(hauksbee_engine::scheduler::OPEN_DRIVER)
        };
        let cosim_holes: Vec<&String> = result
            .coverage_warnings
            .iter()
            .filter(|w| !model_gap(w))
            .collect();
        assert!(
            cosim_holes.is_empty(),
            "{tag}: no co-sim coverage hole in the report, got {cosim_holes:?}\n{human}"
        );
        // Every scaffolded supply is behavioral, which is what makes the rail
        // checks above capable of failing.
        assert!(
            spec.supplies.iter().all(|s| s.kind != "ideal"),
            "{tag}: no scaffolded supply leg is ideal, got {:?}",
            spec.supplies
                .iter()
                .map(|s| s.kind.as_str())
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn init_refuses_to_overwrite_an_existing_spec() {
    let board = copy_board_to_tempdir(&blinky(), "overwrite");
    init_to(&board, board.parent()).expect("first init writes the spec");
    let err = init_to(&board, board.parent()).expect_err("second init must refuse to overwrite");
    assert!(
        err.to_string().contains("refusing to overwrite"),
        "the refusal names the reason, got: {err}"
    );
}

/// stormduino feeds two LDOs from /VIN, which only an open P-FET connects to
/// the USB rail: with no source on /VIN a first run is INVALID. The scaffold
/// declares one (and none on the +5V rail U2 makes, which would fight it), so
/// the first spec is judged.
#[test]
fn init_powers_a_regulator_input_nothing_else_supplies() {
    let board = repo("examples/board-as-code/stormduino.board");
    let scaffold = hauksbee_ci::init::scaffold(&board).expect("scaffold the board");
    let nets: Vec<(&str, &str)> = scaffold
        .supplies
        .iter()
        .map(|s| (s.net.as_str(), s.origin))
        .collect();
    assert_eq!(
        nets,
        [("/VBUS", "rail"), ("/VIN", "regulator_input")],
        "the USB rail, then the one input that needs a source: {:?}",
        scaffold.supplies
    );
    let vin = &scaffold.supplies[1];
    assert_eq!(vin.kind, "bench");
    assert!(
        vin.volts >= 6.0,
        "above the AMS1117-5.0 dropout, got {} V",
        vin.volts
    );

    let text = hauksbee_ci::init::render_spec(&board).expect("render the starter");
    assert!(
        text.contains("# detected regulator input; set your adapter voltage"),
        "the seeded input says what to change, got:\n{text}"
    );
    let spec_path =
        std::env::temp_dir().join(format!("hauksbee_ci_init_{}_vin.toml", std::process::id()));
    std::fs::write(&spec_path, &text).unwrap();
    let spec = Spec::load(&spec_path).expect("the scaffold with its regulator input loads");
    assert!(
        spec.supplies.iter().any(|s| s.net == "/VIN"),
        "/VIN is a live supply"
    );
}

/// `init --json` is what the web builder seeds its supply rows from: the same
/// list, as data, with no file written.
#[test]
fn init_json_lists_the_supplies_and_writes_nothing() {
    let board = copy_board_to_tempdir(&repo("examples/board-as-code/stormduino.board"), "json");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_hauksbee-ci"))
        .args(["init", "--json"])
        .arg(&board)
        .output()
        .expect("run hauksbee-ci init --json");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let doc: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("stdout is one JSON document");
    assert_eq!(doc["ok"], true);
    let supplies = doc["supplies"].as_array().expect("a supplies array");
    assert!(
        supplies.iter().any(|s| s["net"] == "/VIN"
            && s["kind"] == "bench"
            && s["origin"] == "regulator_input"),
        "{doc}"
    );
    assert!(
        !board.with_extension("toml").exists(),
        "--json is a query: it must not write the spec"
    );
}

/// With nothing powering /VIN, the run asks for ONE supply: the +5V rail U2
/// makes feeds U3, and a source there would fight U2's output, so only the
/// first stage of the cascade is asked for.
#[test]
fn an_unpowered_cascade_asks_for_its_first_stage_only() {
    use hauksbee_ir::evidence::Remedy;
    let board = repo("examples/board-as-code/stormduino.board");
    let text = format!(
        "name = \"bare\"\nboard = \"{}\"\nduration_ms = 1\n\n[[assert]]\nkind = \"no_faults\"\n",
        board.display()
    );
    let path = std::env::temp_dir().join(format!("hauksbee_ci_bare_{}.toml", std::process::id()));
    std::fs::write(&path, text).unwrap();
    let spec = Spec::load(&path).expect("a bare spec loads");
    let outcomes = hauksbee_ci::runner::run_spec(&spec).expect("it runs");
    let asked: Vec<String> = outcomes
        .iter()
        .filter_map(|o| o.evidence.as_ref())
        .flat_map(|e| e.assumptions().iter())
        .filter_map(|a| match a.remedy() {
            Remedy::AddSupply { net, .. } => Some(net.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        asked,
        ["/VIN"],
        "only the cascade's first stage is asked for"
    );
}
