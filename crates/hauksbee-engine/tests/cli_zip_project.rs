//! A `KiCad` project shipped only inside a `.zip` is read by `hauksbee run` and
//! `hauksbee check` as if it had been unpacked: the same board, the same
//! project settings, and clear errors for the archives that cannot be read.

use crate::support::{run, run_board};
use std::io::Write;
use std::path::{Path, PathBuf};

const PCB: &[u8] = include_bytes!("../../hauksbee-ci/examples/boards/watchy.kicad_pcb");
const PRO: &[u8] = include_bytes!("../../hauksbee-ci/examples/boards/watchy.kicad_pro");

fn scratch(tag: &str) -> PathBuf {
    hauksbee_testkit::fresh_scratch_dir("cli_zip_project", tag)
}

fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
    let mut w = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    for (name, bytes) in entries {
        w.start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        w.write_all(bytes).unwrap();
    }
    w.finish().unwrap();
}

/// The report with the board's own path, which differs by construction between
/// the zip and the unpacked copy, taken out.
fn report(out: &std::process::Output, path: &Path) -> String {
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let file = path.file_name().unwrap().to_str().unwrap();
    text.replace(path.to_str().unwrap(), "<board>")
        .replace(file, "<board>")
}

#[test]
fn a_project_zip_reports_the_same_as_the_unpacked_project() {
    let dir = scratch("same_as_dir");
    let loose = dir.join("loose");
    std::fs::create_dir_all(&loose).unwrap();
    let loose_board = loose.join("watchy.kicad_pcb");
    std::fs::write(&loose_board, PCB).unwrap();
    std::fs::write(loose.join("watchy.kicad_pro"), PRO).unwrap();
    // A wrapper folder, the way a repository download arrives.
    let zip = dir.join("watchy.zip");
    write_zip(
        &zip,
        &[
            ("watchy-main/README.md", b"readme"),
            ("watchy-main/hardware/watchy.kicad_pcb", PCB),
            ("watchy-main/hardware/watchy.kicad_pro", PRO),
        ],
    );

    let from_dir = run_board(&loose_board, &["--drc"]);
    let from_zip = run_board(&zip, &["--drc"]);
    assert!(
        from_zip.status.success(),
        "zip run failed: {}",
        String::from_utf8_lossy(&from_zip.stderr)
    );
    assert_eq!(
        report(&from_zip, &zip),
        report(&from_dir, &loose_board),
        "a zipped project must read exactly as the unpacked one, project rules included"
    );

    let check_dir = run(&["check", loose_board.to_str().unwrap(), "--plain"]);
    let check_zip = run(&["check", zip.to_str().unwrap(), "--plain"]);
    assert_eq!(check_zip.status.code(), check_dir.status.code());
    assert_eq!(
        report(&check_zip, &zip),
        report(&check_dir, &loose_board),
        "check on a project zip must read as the unpacked project"
    );
}

#[test]
fn a_zip_with_several_projects_lists_them_and_asks_for_one() {
    let dir = scratch("several");
    let zip = dir.join("many.zip");
    write_zip(
        &zip,
        &[
            ("a/one.kicad_pro", b"{}"),
            ("a/one.kicad_pcb", PCB),
            ("b/two.kicad_pro", b"{}"),
            ("b/two.kicad_pcb", PCB),
        ],
    );
    let out = run_board(&zip, &["--check"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("a/one.kicad_pro") && err.contains("b/two.kicad_pro"),
        "{err}"
    );
    assert!(err.contains("one design at a time"), "{err}");
}

#[test]
fn a_zip_with_no_design_source_says_a_schematic_or_pcb_is_needed() {
    let dir = scratch("no_source");
    let zip = dir.join("job.zip");
    write_zip(
        &zip,
        &[("README.md", b"not a board"), ("notes.txt", b"hello")],
    );
    let out = run_board(&zip, &["--check"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("schematic or PCB source is needed"), "{err}");
}

#[test]
fn a_zip_member_that_escapes_the_folder_is_refused() {
    let dir = scratch("slip");
    let zip = dir.join("slip.zip");
    write_zip(&zip, &[("../evil.kicad_pro", b"{}"), ("ok.kicad_pcb", PCB)]);
    let out = run_board(&zip, &["--check"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("escapes"));
    assert!(!dir.join("evil.kicad_pro").exists());
    assert!(!dir.parent().unwrap().join("evil.kicad_pro").exists());
}
