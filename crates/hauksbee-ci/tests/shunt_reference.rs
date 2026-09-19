//! A TL431 is an adjustable SHUNT reference: the BOARD's series resistor and
//! R1/R2 divider set its cathode voltage, and `bind_shunt_ref` stamps it from
//! four primitives the IR already has so the datasheet's
//! `VKA = VREF*(1 + R1/R2) + IREF*R1` is SOLVED, not printed. Every band is set
//! from ngspice-47 on `tests/fixtures/tl431_hauksbee_form.cir`, the same four
//! devices written as ngspice cards:
//! 5.008696 V for the 5 V setter (the ideal equation says 5.010000; the 1.3 mV
//! is the finite gain the datasheet `|ZKA|` fixes), 2.494851 V two-terminal,
//! 1.983333 V starved. That is numerical agreement on the SAME model form, NOT
//! physical validation of a TL431.

use crate::support;

/// A package: its footprint, and the pads SLVS543 Table 5-1 gives that package
/// for CATHODE / REF / ANODE. The pin map is package-dependent and the orders
/// are NOT rotations of one another, so the pads are part of the case.
type Pkg = (&'static str, [u32; 3]);
const DBZ: Pkg = ("Package_TO_SOT_SMD:SOT-23", [1, 2, 3]);
const LP: Pkg = ("Package_TO_SOT_THT:TO-92_Inline", [1, 3, 2]);

/// U1 (TL431 in package `pkg`), `rs` from VIN to the cathode net OUT and, when
/// `div`, R1/R2 of 10 k each. `refnet` is the REF pad's net: "REF" programmed,
/// "OUT" two-terminal 2.5 V, "GND" the mis-wire.
fn board(rs: &str, div: bool, refnet: &str, (lib, [k, f, a]): Pkg) -> String {
    let pad = |n: u32, net: &str| {
        format!("pad \"{n}\" smd rect at {n} 0 size 2 1.5 layers [F.Cu F.Mask] net \"{net}\"\n")
    };
    let comp = |r: &str, val: &str, x: usize, lib: &str, pads: String| {
        format!(
            "comp {r} lib \"{lib}\" val \"{val}\" layer \"F.Cu\" at {x} 20 rot 0 {{\n{pads}}}\n"
        )
    };
    let u1 = pad(k, "OUT") + &pad(f, refnet) + &pad(a, "GND");
    let mut parts = comp("U1", "TL431", 20, lib, u1);
    for (i, (r, val, a, b)) in [
        ("RS", Some(rs), "VIN", "OUT"),
        ("R1", div.then_some("10k"), "OUT", "REF"),
        ("R2", div.then_some("10k"), "REF", "GND"),
    ]
    .into_iter()
    .enumerate()
    {
        let Some(val) = val else { continue };
        let r0805 = "Resistor_SMD:R_0805_2012Metric";
        parts += &comp(r, val, 40 + i * 8, r0805, pad(1, a) + &pad(2, b));
    }
    // An empty REF net would be a floating-net finding of the test's own making.
    let refnet = if div { "net \"REF\"\n" } else { "" };
    format!("board version 20171130\nfn main {{\nnet \"VIN\"\nnet \"OUT\"\nnet \"GND\"\n{refnet}{parts}}}\n")
}

/// Run one board with one cathode-net voltage assertion, check what the report
/// tells its reader about U1, and return whether the assertion passed.
fn vka_in(case: &str, rs: &str, div: bool, refnet: &str, (min, max): (f64, f64), pkg: Pkg) -> bool {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("tl431.board");
    std::fs::write(&path, board(rs, div, refnet, pkg)).expect("write board");
    let r = support::run_body(
        &format!("{case}.toml"),
        &format!(
            "name = \"{case}\"\nboard = {}\nduration_ms = 3\n\
             [[supply]]\nnet = \"VIN\"\nkind = \"ideal\"\nvolts = 12.0\n\
             [[assert]]\nkind = \"voltage\"\nname = \"vka\"\nnet = \"OUT\"\n\
             after_ms = 1.0\nmin = {min}\nmax = {max}\n",
            support::toml_path(&path),
        ),
    );
    // `warning` -> `[partial model]` -> `Assumption` is the only route reaching
    // a reader, so the refusal and the NOT-MODELLED gaps are checked THERE.
    let human = r.render_human().to_lowercase();
    let want = if refnet == "GND" {
        "[open-part:u1]|tied to its anode"
    } else {
        "modelled only in part|temperature is not modelled|capacitive load|minimum cathode current"
    };
    let missing = want.split('|').find(|w| !human.contains(w));
    assert!(missing.is_none(), "{case}: report never said {missing:?}");
    // SOLVED, not declared: on every starved board (9k1, 100k), on no other.
    let flag = human.contains("requires for regulation");
    assert_eq!(flag, rs != "1k", "{case}: min-current flag");
    r.results
        .iter()
        .find(|x| x.label == "vka")
        .unwrap_or_else(|| panic!("{case}: no vka result in {:?}", r.results))
        .passed
}

/// The held-out prediction challenge's own acceptance band for the 5 V setter.
const ACCEPT_5V: (f64, f64) = (4.835, 5.085);

#[test]
fn a_shunt_reference_is_programmed_by_the_board_and_sags_when_it_is_starved() {
    // (case, RS, divider, REF net, band, must pass). Row 2 holds to +/-5 mV of
    // ngspice; row 4 regulates but under IKmin; rows 5/6 are the starved pair,
    // missing the band and landing on the bare divider; row 7 answers nothing.
    for (case, rs, div, refnet, band, want) in [
        ("5v accept", "1k", true, "REF", ACCEPT_5V, true),
        ("5v ngspice", "1k", true, "REF", (5.0037, 5.0137), true),
        ("2v5 direct", "1k", false, "OUT", (2.4899, 2.4999), true),
        ("marginal", "9k1", true, "REF", ACCEPT_5V, true),
        ("starved claim", "100k", true, "REF", ACCEPT_5V, false),
        ("starved sag", "100k", true, "REF", (1.9633, 2.0033), true),
        ("ref on anode", "1k", false, "GND", ACCEPT_5V, false),
    ] {
        let got = vka_in(case, rs, div, refnet, band, DBZ);
        assert_eq!(got, want, "{case} {band:?}");
    }
    // THE REGRESSION. The same electrical part in TO-92, whose pads are CATHODE
    // 1 / ANODE 2 / REF 3 -- a DIFFERENT order, not a rotation. One card with
    // one pin map cannot regulate on both boards, so a package-blind map fails
    // here even though it passes every row above.
    for (case, band) in [
        ("to92 accept", ACCEPT_5V),
        ("to92 ngspice", (5.0037, 5.0137)),
    ] {
        assert!(vka_in(case, "1k", true, "REF", band, LP), "{case} {band:?}");
    }
}
