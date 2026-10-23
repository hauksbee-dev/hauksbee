//! The clearance sweep judges a board against its own rules: the netclass
//! values, the board minimum, a pad's or footprint's own clearance, and the
//! `.kicad_dru` custom rules (condition, layer scope, severity). Every
//! firing case has a silent twin, and the table at the end is checked against
//! `kicad-cli pcb drc` where that is installed.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::process::Command;

use hauksbee_extract::{
    clearance_rules_from_kicad_pro, parse_kicad_dru, DrcReport, ExtractedBoard,
};

const HEAD: &str = "(kicad_pcb
	(version 20240108)
	(generator pcbnew)
	(general (thickness 1.6))
	(layers
		(0 \"F.Cu\" signal)
		(31 \"B.Cu\" signal)
		(36 \"B.SilkS\" user \"b.silkscreen\")
		(37 \"F.SilkS\" user \"f.silkscreen\")
		(44 \"Edge.Cuts\" user)
	)
	(setup (pad_to_mask_clearance 0))
	(net 0 \"\")
	(net 1 \"A\")
	(net 2 \"B\")
	(net 3 \"GND\")
";

fn board(items: &str) -> String {
    format!("{HEAD}{items})\n")
}

/// A project with a Default netclass of `default_mm` and a board minimum.
fn project(default_mm: f64, min_clearance_mm: f64) -> String {
    format!(
        r#"{{
  "board": {{"design_settings": {{"rules": {{"min_clearance": {min_clearance_mm}}}, "rule_severities": {{"clearance": "error", "shorting_items": "error", "unconnected_items": "ignore"}}}}}},
  "meta": {{"filename": "t.kicad_pro", "version": 3}},
  "net_settings": {{"classes": [{{"clearance": {default_mm}, "name": "Default", "priority": 2147483647, "track_width": 0.2}}],
   "meta": {{"version": 5}}, "netclass_assignments": null, "netclass_patterns": []}}
}}"#
    )
}

fn seg(x1: f64, y1: f64, x2: f64, y2: f64, net: u8, layer: &str) -> String {
    format!(
        "(segment (start {x1} {y1}) (end {x2} {y2}) (width 0.2) (layer \"{layer}\") (net {net}))\n"
    )
}

fn net_name(net: u8) -> &'static str {
    ["", "A", "B", "GND"][usize::from(net)]
}

fn pad(number: &str, x: f64, net: u8, local_clearance: Option<f64>, through_hole: bool) -> String {
    let (kind, layers, drill) = if through_hole {
        ("thru_hole", "\"*.Cu\"", " (drill 0.5)")
    } else {
        ("smd", "\"F.Cu\"", "")
    };
    let local = local_clearance.map_or_else(String::new, |c| format!(" (clearance {c})"));
    format!(
        "(pad \"{number}\" {kind} rect (at {x} 0) (size 1 1){drill} (layers {layers}) (net {net} \"{}\"){local})\n",
        net_name(net)
    )
}

fn footprint(reference: &str, x: f64, pads: &str) -> String {
    format!(
        "(footprint \"lib:fp\" (layer \"F.Cu\") (at {x} 0)\n (property \"Reference\" \"{reference}\" (at 0 0))\n{pads})\n"
    )
}

/// Two parallel tracks on nets A and B, 0.15 mm apart edge to edge.
fn two_tracks() -> String {
    seg(0.0, 0.0, 10.0, 0.0, 1, "F.Cu") + &seg(0.0, 0.35, 10.0, 0.35, 2, "F.Cu")
}

fn dru(rules: &str) -> String {
    format!("(version 1)\n{rules}\n")
}

fn rule(extra: &str, condition: Option<&str>, min: &str) -> String {
    let condition = condition.map_or_else(String::new, |c| format!("(condition \"{c}\")"));
    dru(&format!(
        "(rule r {extra} {condition} (constraint clearance (min {min})))"
    ))
}

fn judge(items: &str, project_json: &str, rules: Option<&str>) -> DrcReport {
    let mut project_rules =
        clearance_rules_from_kicad_pro(project_json, ["A", "B", "GND"]).expect("project parses");
    if let Some(rules) = rules {
        let parsed = parse_kicad_dru(rules).expect("rules parse");
        let custom = parsed.clearance_rules();
        if !custom.is_empty() {
            project_rules.set_custom_rules(custom);
        }
        if let Some(global) = parsed.global_clearance_mm() {
            project_rules.apply_global_clearance_override(global);
        }
    }
    ExtractedBoard::drc_with_clearance_rules(&board(items), Some(project_rules)).expect("DRC runs")
}

fn fires(items: &str, project_json: &str, rules: Option<&str>) -> bool {
    judge(items, project_json, rules)
        .clearance_violations()
        .count()
        > 0
}

#[test]
fn the_board_default_governs_when_it_has_no_custom_rules() {
    let p = project(0.2, 0.0);
    assert!(
        fires(&two_tracks(), &p, None),
        "0.15 mm gap vs 0.20 mm class"
    );
    assert!(
        !fires(&two_tracks(), &project(0.1, 0.0), None),
        "0.15 mm gap vs 0.10 mm class"
    );
}

#[test]
fn a_conditional_rule_governs_only_the_pairs_it_selects() {
    let p = project(0.2, 0.0);
    let tracks = rule("", Some("A.Type == 'Track' && B.Type == 'Track'"), "0.1mm");
    assert!(
        !fires(&two_tracks(), &p, Some(&tracks)),
        "rule selects the pair"
    );
    let vias = rule("", Some("A.Type == 'Via'"), "0.1mm");
    assert!(
        fires(&two_tracks(), &p, Some(&vias)),
        "rule selects other items"
    );
    let net_a = rule("", Some("A.NetName == 'A'"), "0.1mm");
    assert!(
        !fires(&two_tracks(), &p, Some(&net_a)),
        "A is net A in one order"
    );
    let net_gnd = rule("", Some("A.NetName == 'GND'"), "0.1mm");
    assert!(fires(&two_tracks(), &p, Some(&net_gnd)));
}

#[test]
fn a_layer_scoped_rule_governs_only_that_layer() {
    let p = project(0.2, 0.0);
    let front = dru("(rule r (layer \"F.Cu\") (constraint clearance (min 0.1mm)))");
    assert!(!fires(&two_tracks(), &p, Some(&front)));
    let back = dru("(rule r (layer \"B.Cu\") (constraint clearance (min 0.1mm)))");
    assert!(fires(&two_tracks(), &p, Some(&back)));
    // KiCad reads a quoted "outer" as a layer name, not the keyword, and
    // discards the whole rules file over it: the rule is not guessed at.
    let quoted = dru("(rule r (layer \"outer\") (constraint clearance (min 0.1mm)))");
    let report = judge(&two_tracks(), &p, Some(&quoted));
    assert_eq!(report.clearance_violations().count(), 0);
    assert!(!report.clearance_unjudged.is_empty());
    let keyword = dru("(rule r (layer outer) (constraint clearance (min 0.1mm)))");
    assert!(!fires(&two_tracks(), &p, Some(&keyword)));
}

#[test]
fn severity_ignore_on_the_winning_rule_suppresses_the_pair() {
    let p = project(0.2, 0.0);
    let ignored = rule("(severity ignore)", Some("A.NetName == 'A'"), "0.1mm");
    assert!(!fires(&two_tracks(), &p, Some(&ignored)));
    let other = rule("(severity ignore)", Some("A.NetName == 'GND'"), "0.1mm");
    assert!(fires(&two_tracks(), &p, Some(&other)));
}

#[test]
fn the_board_minimum_floors_the_netclass_but_not_a_custom_rule() {
    // Netclass 0.1 mm is lifted to the 0.18 mm board minimum.
    assert!(fires(&two_tracks(), &project(0.1, 0.18), None));
    assert!(!fires(&two_tracks(), &project(0.1, 0.1), None));
    // A custom 0.1 mm rule is below the floor and still governs.
    let custom = dru("(rule r (constraint clearance (min 0.1mm)))");
    assert!(!fires(&two_tracks(), &project(0.2, 0.18), Some(&custom)));
    // A stricter custom rule fires.
    let strict = dru("(rule r (constraint clearance (min 0.3mm)))");
    assert!(fires(&two_tracks(), &project(0.2, 0.05), Some(&strict)));
}

#[test]
fn a_pads_own_clearance_outranks_the_custom_rule() {
    let p = project(0.2, 0.0);
    let pads = |local: f64| {
        footprint("U1", 0.0, &pad("1", 0.0, 1, Some(local), false))
            + &footprint("U2", 1.15, &pad("1", 0.0, 2, None, false))
    };
    // Local 0.3 mm beats the looser 0.1 mm rule: the 0.15 mm gap fires.
    let loose = dru("(rule r (constraint clearance (min 0.1mm)))");
    assert!(fires(&pads(0.3), &p, Some(&loose)));
    // Local 0.1 mm beats the stricter 0.3 mm rule: the gap passes.
    let strict = dru("(rule r (constraint clearance (min 0.3mm)))");
    assert!(!fires(&pads(0.1), &p, Some(&strict)));
}

#[test]
fn a_rule_the_reader_cannot_evaluate_withholds_instead_of_guessing() {
    let p = project(0.2, 0.0);
    let opaque = rule("", Some("A.isPlated()"), "0.1mm");
    let report = judge(&two_tracks(), &p, Some(&opaque));
    assert_eq!(report.clearance_violations().count(), 0, "not accused");
    assert!(!report.clearance_unjudged.is_empty(), "and not cleared");
    assert_eq!(report.clearance_unjudged.rules, ["r"]);
    assert!(report.clearance_unjudged.locations >= 1);

    // The same rule on a pair it cannot touch leaves nothing withheld: the
    // pair is judged against the netclass, which fires.
    let supported = rule("", Some("A.NetName == 'GND'"), "0.1mm");
    let report = judge(&two_tracks(), &p, Some(&supported));
    assert!(report.clearance_unjudged.is_empty());
    assert_eq!(report.clearance_violations().count(), 1);
}

#[test]
fn an_unreadable_rule_that_cannot_change_the_verdict_is_not_withheld() {
    let p = project(0.2, 0.0);
    // The 0.15 mm gap fails the 0.2 mm class and would fail 0.3 mm too, so
    // whether the rule applies does not matter: the finding stands.
    let stricter = rule("", Some("A.isPlated()"), "0.3mm");
    let report = judge(&two_tracks(), &p, Some(&stricter));
    assert!(report.clearance_unjudged.is_empty());
    assert_eq!(report.clearance_violations().count(), 1);
    // A 0.25 mm gap passes the class, but the same rule might demand 0.3 mm:
    // that pair is withheld, not cleared.
    let wide = seg(0.0, 0.0, 10.0, 0.0, 1, "F.Cu") + &seg(0.0, 0.45, 10.0, 0.45, 2, "F.Cu");
    let report = judge(&wide, &p, Some(&stricter));
    assert_eq!(report.clearance_violations().count(), 0);
    assert!(!report.clearance_unjudged.is_empty());
}

// ---- kicad-cli as the oracle -------------------------------------------

fn kicad_cli_version() -> Option<String> {
    let out = Command::new("kicad-cli").arg("--version").output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn cache_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("kicad-clearance-oracle");
    std::fs::create_dir_all(&dir).expect("cache dir");
    dir
}

/// How many `clearance` violations `kicad-cli pcb drc` reports for the board.
/// Cached by a hash of the board, project, rules and the tool version, so an
/// unchanged case never runs the tool again.
fn kicad_clearance_count(
    version: &str,
    items: &str,
    project_json: &str,
    rules: Option<&str>,
) -> usize {
    let pcb = board(items);
    let mut hasher = DefaultHasher::new();
    (version, &pcb, project_json, rules).hash(&mut hasher);
    let cached = cache_dir().join(format!("{:016x}.count", hasher.finish()));
    if let Ok(text) = std::fs::read_to_string(&cached) {
        if let Ok(count) = text.trim().parse() {
            return count;
        }
    }
    let work = cache_dir().join(format!("work-{:016x}", hasher.finish()));
    std::fs::create_dir_all(&work).expect("work dir");
    std::fs::write(work.join("t.kicad_pcb"), &pcb).expect("write board");
    std::fs::write(work.join("t.kicad_pro"), project_json).expect("write project");
    if let Some(rules) = rules {
        std::fs::write(work.join("t.kicad_dru"), rules).expect("write rules");
    }
    let report = work.join("k.json");
    let status = Command::new("kicad-cli")
        .args(["pcb", "drc", "--format", "json", "--severity-all", "-o"])
        .arg(&report)
        .arg(work.join("t.kicad_pcb"))
        .output()
        .expect("kicad-cli runs");
    assert!(
        report.is_file(),
        "kicad-cli wrote no report: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&report).expect("report")).expect("json");
    let count = json["violations"].as_array().map_or(0, |all| {
        all.iter().filter(|v| v["type"] == "clearance").count()
    });
    std::fs::write(&cached, count.to_string()).expect("write cache");
    let _ = std::fs::remove_dir_all(&work);
    count
}

#[test]
// One row per board: a table, not logic.
#[allow(clippy::too_many_lines)]
fn clearance_agrees_with_kicad_cli_on_the_boards_own_rules() {
    let Some(version) = kicad_cli_version() else {
        eprintln!("kicad-cli is not installed: oracle comparison skipped");
        return;
    };
    let p = project(0.2, 0.0);
    let via = "(via (at 0 0) (size 0.6) (drill 0.3) (layers \"F.Cu\" \"B.Cu\") (net 1))\n"
        .to_string()
        + &seg(0.9, -3.0, 0.9, 3.0, 2, "F.Cu");
    let pads2 = |local: Option<f64>| {
        footprint("U1", 0.0, &pad("1", 0.0, 1, local, false))
            + &footprint("U2", 1.15, &pad("1", 0.0, 2, None, false))
    };
    let pth = footprint("U1", 0.0, &pad("1", 0.0, 1, None, true))
        + &footprint("U2", 1.15, &pad("1", 0.0, 2, None, true));
    let track_pad = footprint("U1", 0.0, &pad("1", 0.0, 1, None, false))
        + &seg(0.75, -3.0, 0.75, 3.0, 2, "F.Cu");

    let cases: Vec<(&str, String, String, Option<String>)> = vec![
        ("baseline", two_tracks(), p.clone(), None),
        ("class_looser", two_tracks(), project(0.1, 0.0), None),
        ("floor_lifts_class", two_tracks(), project(0.1, 0.18), None),
        (
            "global_rule",
            two_tracks(),
            p.clone(),
            Some(dru("(rule r (constraint clearance (min 0.1mm)))")),
        ),
        (
            "custom_below_floor",
            two_tracks(),
            project(0.2, 0.18),
            Some(dru("(rule r (constraint clearance (min 0.1mm)))")),
        ),
        (
            "custom_above_floor",
            two_tracks(),
            project(0.2, 0.05),
            Some(dru("(rule r (constraint clearance (min 0.3mm)))")),
        ),
        (
            "type_track_pair",
            two_tracks(),
            p.clone(),
            Some(rule(
                "",
                Some("A.Type == 'Track' && B.Type == 'Track'"),
                "0.1mm",
            )),
        ),
        (
            "type_via",
            two_tracks(),
            p.clone(),
            Some(rule("", Some("A.Type == 'Via'"), "0.1mm")),
        ),
        (
            "type_upper",
            two_tracks(),
            p.clone(),
            Some(rule("", Some("A.Type == 'TRACK'"), "0.1mm")),
        ),
        (
            "netname_a",
            two_tracks(),
            p.clone(),
            Some(rule("", Some("A.NetName == 'A'"), "0.1mm")),
        ),
        (
            "netname_lower",
            two_tracks(),
            p.clone(),
            Some(rule("", Some("A.NetName == 'a'"), "0.1mm")),
        ),
        (
            "netname_compare",
            two_tracks(),
            p.clone(),
            Some(rule("", Some("A.NetName == B.NetName"), "0.1mm")),
        ),
        (
            "netname_differ",
            two_tracks(),
            p.clone(),
            Some(rule("", Some("A.NetName != B.NetName"), "0.1mm")),
        ),
        (
            "netclass_default",
            two_tracks(),
            p.clone(),
            Some(rule("", Some("A.NetClass == 'Default'"), "0.1mm")),
        ),
        (
            "layer_front",
            two_tracks(),
            p.clone(),
            Some(dru(
                "(rule r (layer \"F.Cu\") (constraint clearance (min 0.1mm)))",
            )),
        ),
        (
            "layer_back",
            two_tracks(),
            p.clone(),
            Some(dru(
                "(rule r (layer \"B.Cu\") (constraint clearance (min 0.1mm)))",
            )),
        ),
        (
            "layer_outer_keyword",
            two_tracks(),
            p.clone(),
            Some(dru(
                "(rule r (layer outer) (constraint clearance (min 0.1mm)))",
            )),
        ),
        (
            "layer_outer_quoted",
            two_tracks(),
            p.clone(),
            Some(dru(
                "(rule r (layer \"outer\") (constraint clearance (min 0.1mm)))",
            )),
        ),
        (
            "layer_inner",
            two_tracks(),
            p.clone(),
            Some(dru(
                "(rule r (layer inner) (constraint clearance (min 0.1mm)))",
            )),
        ),
        (
            "severity_ignore",
            two_tracks(),
            p.clone(),
            Some(rule("(severity ignore)", Some("A.NetName == 'A'"), "0.1mm")),
        ),
        (
            "severity_ignore_other",
            two_tracks(),
            p.clone(),
            Some(rule(
                "(severity ignore)",
                Some("A.NetName == 'GND'"),
                "0.1mm",
            )),
        ),
        (
            "pad_local_loose_rule",
            pads2(Some(0.3)),
            p.clone(),
            Some(dru("(rule r (constraint clearance (min 0.1mm)))")),
        ),
        (
            "pad_local_tight_rule",
            pads2(Some(0.1)),
            p.clone(),
            Some(dru("(rule r (constraint clearance (min 0.3mm)))")),
        ),
        (
            "pad_pair_rule_pad",
            track_pad.clone(),
            p.clone(),
            Some(rule("", Some("A.Type == 'Pad'"), "0.1mm")),
        ),
        (
            "pad_pair_rule_track",
            track_pad.clone(),
            p.clone(),
            Some(rule("", Some("A.Type == 'Track'"), "0.1mm")),
        ),
        (
            "pad_pair_rule_via",
            track_pad.clone(),
            p.clone(),
            Some(rule("", Some("A.Type == 'Via'"), "0.1mm")),
        ),
        (
            "pad_pair_pad_and_track",
            track_pad.clone(),
            p.clone(),
            Some(rule(
                "",
                Some("A.Type == 'Pad' && B.Type == 'Track'"),
                "0.1mm",
            )),
        ),
        (
            "pad_pair_pad_and_pad",
            track_pad,
            p.clone(),
            Some(rule(
                "",
                Some("A.Type == 'Pad' && B.Type == 'Pad'"),
                "0.1mm",
            )),
        ),
        (
            "via_rule_via",
            via.clone(),
            p.clone(),
            Some(rule("", Some("A.Type == 'Via'"), "0.1mm")),
        ),
        (
            "via_rule_track",
            via,
            p.clone(),
            Some(rule("", Some("A.Type == 'Track'"), "0.1mm")),
        ),
        (
            "pth_rule_back_layer",
            pth.clone(),
            p.clone(),
            Some(rule("", Some("A.Layer == 'B.Cu'"), "0.1mm")),
        ),
        (
            "pth_rule_front_layer",
            pth,
            p.clone(),
            Some(rule("", Some("A.Layer == 'F.Cu'"), "0.1mm")),
        ),
        (
            "member_of_ref",
            pads2(None),
            p.clone(),
            Some(rule("", Some("A.memberOfFootprint('U1')"), "0.1mm")),
        ),
        (
            "member_of_other",
            pads2(None),
            p.clone(),
            Some(rule("", Some("A.memberOfFootprint('U3')"), "0.1mm")),
        ),
        (
            "opaque_plated",
            two_tracks(),
            p,
            Some(rule("", Some("A.isPlated()"), "0.1mm")),
        ),
    ];

    // The oracle runs are independent: four at a time.
    let version = &version;
    let kicad_fires: Vec<bool> = std::thread::scope(|scope| {
        // Every worker is started before the first is joined.
        #[allow(clippy::needless_collect)]
        let handles: Vec<_> = cases
            .chunks(cases.len().div_ceil(4))
            .map(|chunk| {
                scope.spawn(move || {
                    chunk
                        .iter()
                        .map(|(_, items, project_json, rules)| {
                            kicad_clearance_count(version, items, project_json, rules.as_deref())
                                > 0
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("oracle worker"))
            .collect()
    });

    let mut disagreements = Vec::new();
    for ((name, items, project_json, rules), kicad) in cases.iter().zip(kicad_fires) {
        let ours = judge(items, project_json, rules.as_deref());
        let accuses = ours.clearance_violations().count() > 0;
        let withheld = !ours.clearance_unjudged.is_empty();
        // Sound both ways: never accuse where KiCad passes; never pass where
        // KiCad fires unless the pair was disclosed as not judged.
        let ok = if kicad { accuses || withheld } else { !accuses };
        if !ok {
            disagreements.push(format!(
                "{name}: kicad fires={kicad}, ours accuses={accuses} withheld={withheld}"
            ));
        }
    }
    assert!(disagreements.is_empty(), "{}", disagreements.join("\n"));
}
