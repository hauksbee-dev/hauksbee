use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Write `body` to a fresh temp file named `name` and return its path.
///
/// Every integration module compiles into the same `it` binary and cargo
/// runs its tests concurrently, so the destination directory is unique per
/// call (process id + a monotonic counter) rather than shared by name: two
/// tests writing the same `name` (even across different modules) must never
/// collide on one path.
pub fn write_tmp(name: &str, body: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("hauksbee_ci_tests_{}_{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    p
}

/// One `KiCad` pad: number `n` on net `net`, placed at x = `n` mm. Lands are
/// 0.6 mm wide on that 1 mm pitch, so neighbouring pads of a part keep a 0.4 mm
/// gap: the fixture's copper is physically valid, never a short the static
/// check would rightly flag. The net id is a placeholder that [`board_text`]
/// numbers once it has seen every net of the board.
pub fn pad(n: u32, net: &str) -> String {
    format!(
        "(pad {n} smd rect (at {n} 0 0) (size 0.6 1.5) (layers \"F.Cu\" \"F.Mask\") (net 0 \"{net}\"))\n"
    )
}

/// Why a placement-only fixture's "not routed" finding is waived: the board
/// places parts and joins nets but carries no copper, so it is a pre-layout
/// board by construction. The committed fixtures directory carries the same
/// waiver in its `hauksbee-waivers.toml`.
pub const PLACEMENT_ONLY_REASON: &str = "placement-only fixture: parts are placed and netted \
     but carry no copper, so the board is pre-layout by construction";

/// Why a hand-written layout fixture's "not routed" finding is waived: its
/// suite judges the netlist, so the parts are placed and netted but left
/// unrouted on purpose.
pub const PRE_LAYOUT_REASON: &str = "pre-layout fixture: placed and netted but left \
     unrouted on purpose, since this suite judges the netlist";

/// Write a placement-only board `name` with `body` (see [`board_text`]),
/// beside the waiver for its not-routed finding (see
/// [`PLACEMENT_ONLY_REASON`]). The waived finding stays a visible row of every
/// run; [`assert_placement_waived`] checks that.
pub fn placement_board(name: &str, body: &str) -> PathBuf {
    let board = write_tmp(name, body);
    let mut nets: Vec<&str> = body
        .split("(net ")
        .skip(1)
        .filter_map(|rest| rest.split('"').nth(1))
        .filter(|n| !n.is_empty())
        .collect();
    nets.sort_unstable();
    nets.dedup();
    write_unrouted_waiver(&board, &nets, PLACEMENT_ONLY_REASON);
    board
}

/// Write the waiver for `board`'s not-routed finding beside it. A waiver names
/// a net the finding must touch, and the finding lists only the nets with pads
/// to join, so there is one waiver per net in `nets` (every net the board
/// declares): whichever the finding lists, one matches.
pub fn write_unrouted_waiver(board: &Path, nets: &[&str], reason: &str) {
    let waivers: String = nets
        .iter()
        .map(|net| waive("lint", "unrouted_pad", net, reason, "2030-01-01"))
        .collect();
    std::fs::write(board.with_file_name("hauksbee-waivers.toml"), waivers).unwrap();
}

/// Assert a pre-layout board's run still shows its not-routed finding: a
/// failed `static_check` row, marked waived with a pre-layout reason (either
/// [`PLACEMENT_ONLY_REASON`] or [`PRE_LAYOUT_REASON`]), not dropped.
pub fn assert_placement_waived(result: &hauksbee_ci::CiResult) {
    use hauksbee_ir::vocabulary::STATIC_CHECK_KIND;
    let shown = result.results.iter().any(|r| {
        r.kind == STATIC_CHECK_KIND
            && r.label
                .starts_with(&format!("{STATIC_CHECK_KIND}: lint unrouted_pad"))
            && !r.passed
            && r.waived
                .as_deref()
                .is_some_and(|w| w.contains("pre-layout"))
    });
    assert!(
        shown,
        "the waived not-routed finding must stay a visible row: {:?}",
        result.results
    );
}

/// One `KiCad` footprint: reference `r`, library id `lib`, value `val`, placed
/// at x = `x`, carrying `pads` (built from [`pad`]).
// Callers hand over freshly built pad strings; owning them keeps call sites borrow-free.
#[allow(clippy::needless_pass_by_value)]
pub fn comp(r: &str, lib: &str, val: &str, x: usize, pads: String) -> String {
    format!(
        "(footprint \"{lib}\" (layer \"F.Cu\") (at {x} 20 0) (property \"Reference\" \"{r}\") (property \"Value\" \"{val}\")\n{pads})\n"
    )
}

/// A whole `.kicad_pcb` text from footprints built with [`comp`]: the board
/// header, one `(net id "name")` row per net, and `parts` with every pad's
/// placeholder net id replaced by that net's number.
///
/// `declared` lists nets the board names even when no pad joins them (a net a
/// spec asserts on, for instance); every net a pad names is declared too.
pub fn board_text(declared: &[&str], parts: &str) -> String {
    let mut nets: Vec<String> = declared.iter().map(|n| (*n).to_string()).collect();
    for rest in parts.split("(net 0 \"").skip(1) {
        if let Some(name) = rest.split('"').next() {
            if !nets.iter().any(|n| n == name) {
                nets.push(name.to_string());
            }
        }
    }
    let mut parts = parts.to_string();
    let mut header = String::new();
    for (i, name) in nets.iter().enumerate() {
        let id = i + 1;
        let _ = writeln!(header, "(net {id} \"{name}\")");
        parts = parts.replace(
            &format!("(net 0 \"{name}\")"),
            &format!("(net {id} \"{name}\")"),
        );
    }
    format!(
        "(kicad_pcb (version 20171130) (generator \"pcbnew\")\n\
         (general (thickness 1.6)) (paper \"A4\")\n\
         (layers (0 \"F.Cu\" signal) (31 \"B.Cu\" signal) (36 \"B.SilkS\" user) \
         (37 \"F.SilkS\" user) (38 \"B.Mask\" user) (39 \"F.Mask\" user) \
         (44 \"Edge.Cuts\" user) (48 \"B.Fab\" user) (49 \"F.Fab\" user))\n\
         (setup (pad_to_mask_clearance 0))\n\
         (net 0 \"\")\n{header}{parts})\n"
    )
}

/// Write `body` to a spec file named `name` and load it, which must succeed.
pub fn load_spec(name: &str, body: &str) -> hauksbee_ci::Spec {
    hauksbee_ci::Spec::load(&write_tmp(name, body)).expect("spec loads")
}

/// Write `body` to a spec file named `name` and return why it refuses to load.
pub fn load_error(name: &str, body: &str) -> String {
    let err = hauksbee_ci::Spec::load(&write_tmp(name, body));
    err.expect_err("spec must refuse to load").to_string()
}

/// Write `body` to a spec file named `name` and run it with default config.
pub fn run_body(name: &str, body: &str) -> hauksbee_ci::CiResult {
    hauksbee_ci::run(&run_config(write_tmp(name, body))).expect("spec runs")
}

/// A [`hauksbee_ci::CiResult`] carrying `results` and nothing else: spec "t" on
/// board "`b.kicad_pcb`", one seed, no elapsed time, and every honesty list empty.
///
/// The report surfaces are judged from synthesized results in several modules,
/// and each one needs the same otherwise-empty carrier around them.
pub fn ci_result(
    results: Vec<hauksbee_ci::assertions::AssertResult>,
    analog_abort: bool,
) -> hauksbee_ci::CiResult {
    hauksbee_ci::CiResult {
        spec_name: "t".to_string(),
        board: "b.kicad_pcb".to_string(),
        results,
        seeds: 1,
        analog_abort,
        ..Default::default()
    }
}

/// Assert the run is GREEN as a user sees it: exit code 0.
///
/// `CiResult::passed()` is only the numeric judgement. It stays true when an
/// assertion's evidence is undermined (INVALID, exit 3), so a test claiming
/// Whether `condition` is one the engine derives from the assumptions an
/// assertion's evidence cites (`verdict_conditions::assumption_conditions`):
/// parts with no model, modelling assumptions, checks not run, reader gaps,
/// parts assumed fitted. Not a sweep, clock or time-bound condition.
pub fn is_evidence_condition(condition: &str) -> bool {
    use hauksbee_ir::vocabulary::{
        MODELLING_ASSUMPTIONS_LEAD, NOT_JUDGED_BY_RUN_LEAD, READER_LIMITATION_LEAD,
    };
    [
        MODELLING_ASSUMPTIONS_LEAD,
        NOT_JUDGED_BY_RUN_LEAD,
        READER_LIMITATION_LEAD,
    ]
    .iter()
    .any(|lead| condition.starts_with(lead))
        || condition.contains(" no model and w")
        || condition.starts_with("Every placed part was judged as fitted")
}

/// "green" must check `exit_code()`. On failure this prints the verdict, every
/// INVALID row's reason and the full human report.
pub fn assert_green(result: &hauksbee_ci::CiResult, what: &str) {
    if result.exit_code() == 0 {
        return;
    }
    let invalid: String =
        result
            .results
            .iter()
            .filter(|r| r.invalid)
            .fold(String::new(), |mut out, r| {
                let _ = write!(out, "\n  INVALID {}: {}", r.label, r.detail);
                out
            });
    panic!(
        "{what}: not green, exit {} (passed()={}, refusal: {:?}){invalid}\n{}",
        result.exit_code(),
        result.passed(),
        result.verdict_reason(),
        result.render_human()
    );
}

/// Assert a run whose own assertions all hold is RED only because the board's
/// static check fails it, on exactly the `static_check` rows whose labels
/// start with `expected`: every other row is GREEN, each expected finding has
/// its failing (unwaived) row, and no other row fails.
pub fn assert_red_only_on_static(result: &hauksbee_ci::CiResult, expected: &[&str], what: &str) {
    use hauksbee_ir::vocabulary::STATIC_CHECK_KIND;
    assert_eq!(result.exit_code(), 1, "{what}: {}", result.render_human());
    for r in result
        .results
        .iter()
        .filter(|r| r.kind != STATIC_CHECK_KIND)
    {
        assert_row_green(r, what);
    }
    let failing: Vec<&str> = result
        .results
        .iter()
        .filter(|r| r.kind == STATIC_CHECK_KIND && !r.passed && r.waived.is_none())
        .map(|r| r.label.as_str())
        .collect();
    for want in expected {
        assert!(
            failing.iter().any(|l| l.starts_with(want)),
            "{what}: no failing row '{want}' in {failing:?}"
        );
    }
    for label in &failing {
        assert!(
            expected.iter().any(|w| label.starts_with(w)),
            "{what}: unexpected static row '{label}'"
        );
    }
}

/// Whether one assertion row is GREEN as a user sees it: it passed AND its
/// evidence is not undermined.
///
/// `AssertResult::passed` is only the numeric judgement. A row can be
/// `invalid == true` while `passed == true` (the numbers are in range but the
/// evidence under them is undermined, e.g. an open part on the path), and the
/// run then exits 3. For use where a test needs a `bool` (a helper returning
/// "did this hold"); prefer [`assert_row_green`] when asserting directly.
pub const fn row_is_green(row: &hauksbee_ci::assertions::AssertResult) -> bool {
    row.passed && !row.invalid
}

/// Assert one assertion row is GREEN as a user sees it: `passed && !invalid`.
///
/// Row-level counterpart of [`assert_green`]. A bare `assert!(row.passed)`
/// counts an INVALID row as a pass; this prints the row's label, both flags
/// and its detail (which carries the INVALID reason) when it fails.
pub fn assert_row_green(row: &hauksbee_ci::assertions::AssertResult, what: &str) {
    assert!(
        row_is_green(row),
        "{what}: row '{}' is not green (passed={}, invalid={}): {}",
        row.label,
        row.passed,
        row.invalid,
        row.detail
    );
}

/// Assert one row is honestly INVALID: the numbers are in range (`passed`) but
/// its evidence is undermined, and the undermined evidence cites an assumption
/// whose id starts with `cause` (for example `open-part:U2` or
/// `operating-limit-violated:U2-logic-level-X-VCC`).
///
/// Row-level counterpart of [`assert_invalid_by_open_parts`]: it pins today's
/// verdict and names the cause, so the test must become [`assert_row_green`]
/// once the cause is closed.
pub fn assert_row_invalid_citing(result: &hauksbee_ci::CiResult, name: &str, cause: &str) {
    let report = result.render_human();
    let r = row(result, name);
    assert!(
        r.passed && r.invalid,
        "'{}' expected passed+INVALID: {r:?}\n{report}",
        r.label
    );
    let map = result
        .evidence
        .iter()
        .find(|m| m.assertion() == r.label)
        .unwrap_or_else(|| panic!("no evidence map for '{}'\n{report}", r.label));
    assert!(
        map.is_undermined(),
        "'{}' INVALID without undermined evidence\n{report}",
        r.label
    );
    let cited = map
        .assumptions()
        .iter()
        .any(|id| id.as_str().starts_with(cause));
    assert!(
        cited,
        "'{}': not undermined by an assumption starting '{cause}'\n{report}",
        r.label
    );
}

/// Assert the run is INVALID (exit 3) *only* because parts with no model are
/// open on the asserted paths, while the numeric judgement (`passed()`) holds.
///
/// For a board with a known model gap: it pins today's honest verdict and
/// names the cause, so the test must become `assert_green` once the part is
/// modelled. Every evidence-undermined assertion must cite at least one of
/// `parts` and no critical cause other than an open part.
pub fn assert_invalid_by_open_parts(result: &hauksbee_ci::CiResult, parts: &[&str]) {
    let report = result.render_human();
    assert_eq!(result.exit_code(), 3, "expected INVALID (exit 3)\n{report}");
    assert_undermined_only_by_open_parts(result, parts);
}

/// The evidence half of [`assert_invalid_by_open_parts`], for a run whose exit
/// code is decided by something else as well (a failing `static_check` row
/// turns exit 3 into exit 1): the simulation's own numbers hold, and every
/// undermined assertion cites an open part from `parts` and no other critical
/// cause.
pub fn assert_undermined_only_by_open_parts(result: &hauksbee_ci::CiResult, parts: &[&str]) {
    use hauksbee_ir::evidence::AssumptionKind::{
        DefaultParameter, InferredPinRole, OpenPart, ReducedFidelity,
    };
    use hauksbee_ir::vocabulary::STATIC_CHECK_KIND;
    let report = result.render_human();
    assert!(
        result
            .results
            .iter()
            .filter(|r| r.kind != STATIC_CHECK_KIND)
            .all(|r| r.passed || r.waived.is_some())
            && !result.analog_abort,
        "numeric judgement\n{report}"
    );
    let undermined: Vec<_> = result
        .evidence
        .iter()
        .filter(|m| m.is_undermined())
        .collect();
    assert!(
        !undermined.is_empty(),
        "INVALID without undermined evidence\n{report}"
    );
    for map in undermined {
        let cited: Vec<_> = map
            .assumptions()
            .iter()
            .filter_map(|id| result.assumptions.iter().find(|a| a.id() == id))
            .collect();
        assert!(
            cited.iter().all(|a| matches!(
                a.kind(),
                OpenPart | ReducedFidelity | DefaultParameter | InferredPinRole
            )),
            "{}: INVALID for a cause other than an open part\n{report}",
            map.assertion()
        );
        let named = cited.iter().any(|a| {
            a.kind() == OpenPart
                && parts
                    .iter()
                    .any(|p| a.id().as_str() == format!("open-part:{p}"))
        });
        assert!(
            named,
            "{}: not undermined by any of {parts:?}\n{report}",
            map.assertion()
        );
    }
}

/// One `[[waive]]` block, as a waiver file's body: the four fields every
/// waiver carries, written once instead of a raw TOML literal per test.
pub fn waive(check: &str, kind: &str, nets: &str, reason: &str, until: &str) -> String {
    format!(
        "\n[[waive]]\ncheck = \"{check}\"\nkind = \"{kind}\"\n\
         nets = [\"{nets}\"]\nreason = \"{reason}\"\nuntil = \"{until}\"\n"
    )
}

/// The one result a test is about: the row whose spec label is `name`, or --
/// with a `kind:` prefix -- the first row of that assertion kind. Panics with
/// the whole list, which is what a test needs to see when its row is missing.
pub fn row<'a>(
    r: &'a hauksbee_ci::CiResult,
    name: &str,
) -> &'a hauksbee_ci::assertions::AssertResult {
    let (by_kind, want) = name
        .strip_prefix("kind:")
        .map_or((false, name), |k| (true, k));
    r.results
        .iter()
        .find(|x| want == if by_kind { &x.kind } else { &x.label })
        .unwrap_or_else(|| panic!("no assertion '{name}' in {:?}", r.results))
}

/// A committed example board/spec under this crate's `examples/` directory.
pub fn example(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join(name)
}

/// A path under the repository root, [`repo_root`] plus `rel`.
pub fn repo(rel: &str) -> PathBuf {
    repo_root().join(rel)
}

/// The directory the board corpus sits directly under, whichever layout this
/// machine has (the hand-built `famous/<id>` tree or the flat `<id>` layout
/// `scripts/fetch-corpus.sh` produces), or an empty default if the corpus is
/// absent.
pub fn corpus_boards_root() -> PathBuf {
    hauksbee_testkit::corpus_boards_root(env!("CARGO_MANIFEST_DIR")).unwrap_or_default()
}

/// Whether an absent corpus board must hard-fail rather than skip.
pub fn require_corpus() -> bool {
    std::env::var("HAUKSBEE_REQUIRE_CORPUS").is_ok()
}

/// Encode a filesystem path as a complete TOML string value.
///
/// Windows paths contain backslashes (and canonical paths may carry the
/// `\\?\` prefix), so interpolating `Path::display()` between quotes creates
/// invalid TOML escape sequences. Let the TOML serializer own the quoting.
pub fn toml_path(path: &Path) -> String {
    toml::Value::String(path.to_string_lossy().into_owned()).to_string()
}

/// A run configuration with only the spec path overridden.
// Some integration modules compile this shared support file without using every helper.
pub fn run_config(spec: PathBuf) -> hauksbee_ci::RunConfig {
    hauksbee_ci::RunConfig {
        spec,
        ..Default::default()
    }
}

/// The compiled `hauksbee-ci` binary (Cargo sets this for the crate's tests).
pub const fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hauksbee-ci")
}

/// The `blinky` example board shipped with this crate.
pub fn blinky_board() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/boards/blinky.kicad_pcb")
}

/// The repo root, two levels up from this crate's manifest dir.
pub fn repo_root() -> PathBuf {
    hauksbee_testkit::repo_root(env!("CARGO_MANIFEST_DIR"))
}

/// Decode a captured `Output`'s stdout as UTF-8.
pub fn stdout(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// Decode a captured `Output`'s stderr as UTF-8.
pub fn stderr(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// Serializes every test that runs a co-sim whose `SoC` descriptor resolution
/// matters.
///
/// Descriptor overrides reach the runner through `HAUKSBEE_MCU_DIR`, which is
/// process-global: a test that publishes a stripped-down descriptor there is
/// visible to every other test running concurrently in this binary. Each such
/// test used to be alone in its own binary, so the isolation was structural;
/// now that they share one, it has to be taken explicitly.
///
/// Take it for the whole body of any test that either publishes a descriptor
/// dir or runs a spec expecting the stock descriptors.
pub static DESCRIPTOR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take [`DESCRIPTOR_ENV_LOCK`], ignoring poisoning from an unrelated test's
/// panic: the guarded resource is the environment, which a panicking test
/// leaves no worse than it found.
pub fn descriptor_env_lock() -> std::sync::MutexGuard<'static, ()> {
    DESCRIPTOR_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The Renode thermostat fixture `(board, firmware)`, or `None` (with a SKIP
/// line) when Renode or either fixture is missing.
#[cfg(feature = "renode")]
pub fn renode_thermostat() -> Option<(PathBuf, PathBuf)> {
    hauksbee_testkit::skip_if!(!hauksbee_mcu::renode::is_available() => None, "Renode not installed");
    let board = repo("testdata/boards/stm32_i2c_thermostat.kicad_pcb");
    let fw = repo("testdata/firmware/stm32_i2c_thermostat/thermostat.elf");
    hauksbee_testkit::skip_if!(!board.exists() || !fw.exists() => None, "thermostat board/firmware not present");
    Some((board, fw))
}
