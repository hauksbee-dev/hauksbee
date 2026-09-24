//! The installed pre-commit hook, proven through a real `git commit`.
//!
//! Everything else about `hook install` can be checked by reading the file it
//! writes. Whether the gate actually FIRES cannot: the bug this file exists for
//! (H1) was a hook that installed cleanly, reported success, read correctly, and
//! never ran, because it was appended after the existing hook's `exit 0`. The
//! only test that catches that is one that stages a change, runs `git commit`,
//! and looks at the exit code.
//!
//! Two-sided on purpose: the hauksbee gate must block a RED spec even with
//! someone else's hook already in place, AND that hook's own logic must still
//! run (installing a hardware gate must not disable the repo's other checks).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::support::blinky_board as board;
use crate::support::{bin, stderr, stdout};

/// A git repo with an identity, so `git commit` works unattended.
fn repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    git(tmp.path(), &["init", "-q"]);
    git(tmp.path(), &["config", "user.email", "ci@example.invalid"]);
    git(tmp.path(), &["config", "user.name", "hauksbee ci test"]);
    git(tmp.path(), &["config", "commit.gpgsign", "false"]);
    tmp
}

/// git's absolute path, for the case where the test blanks PATH to simulate a
/// machine with no hauksbee-ci installed: git still has to be startable.
fn git_binary() -> PathBuf {
    let path = std::env::var_os("PATH").expect("PATH is available");
    let names: &[&str] = if cfg!(windows) {
        &["git.exe", "git.cmd", "git.bat"]
    } else {
        &["git"]
    };
    std::env::split_paths(&path)
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|candidate| candidate.is_file())
        .expect("locate git on PATH")
}

fn git(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .expect("git runs")
}

/// `git commit -m <message>` with the built binary on PATH, so the hook finds
/// `hauksbee-ci` the way a user's shell would, and `HAUKSBEE_CI_SPECS` set to
/// `specs` (or cleared).
fn commit_with(dir: &Path, message: &str, specs: Option<&str>) -> Output {
    let bin_dir = Path::new(bin()).parent().unwrap().to_path_buf();
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let paths = std::iter::once(bin_dir).chain(std::env::split_paths(&inherited));
    let mut cmd = Command::new("git");
    cmd.args(["commit", "-m", message])
        .current_dir(dir)
        .env("PATH", std::env::join_paths(paths).expect("compose PATH"))
        .env_remove("GIT_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("HAUKSBEE_CI_HOOK_OPTIONAL");
    match specs {
        Some(s) => cmd.env("HAUKSBEE_CI_SPECS", s),
        None => cmd.env_remove("HAUKSBEE_CI_SPECS"),
    };
    cmd.output().expect("git commit runs")
}

fn commit(dir: &Path) -> Output {
    commit_with(dir, "hardware change", None)
}

/// An existing pre-commit hook: `body` after the shebang. Each one records
/// that it ran, so the test can tell whether chaining actually happened.
fn write_hook(dir: &Path, body: &str) {
    let hook = dir.join(".git/hooks/pre-commit");
    std::fs::write(
        &hook,
        format!("#!/bin/sh\necho ran > legacy-hook-ran.txt\n{body}"),
    )
    .unwrap();
    make_executable(&hook);
}

/// The canonical existing-hook shape: does its own work, then `exit 0`.
fn write_existing_hook(dir: &Path) {
    write_hook(dir, "# somebody else's gate\nexit 0\n");
}

/// Same shape, but it REJECTS the commit: the chained hook's nonzero exit has
/// to propagate through ours.
fn write_rejecting_hook(dir: &Path) {
    write_hook(dir, "echo 'legacy hook says no' >&2\nexit 1\n");
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).unwrap();
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

/// A spec in `ci/` whose single assertion fails on a real solve: the 5 V rail
/// cannot reach 99 V. Cheap (5 ms of simulated time, no firmware) so the hook
/// under test stays a second, not a minute.
fn write_red_spec(dir: &Path) {
    write_spec(dir, 99.0);
}

/// The same spec with a floor the rail clears: GREEN.
fn write_green_spec(dir: &Path) {
    write_spec(dir, 4.5);
}

fn write_spec(dir: &Path, min_v: f64) {
    std::fs::create_dir_all(dir.join("ci")).unwrap();
    std::fs::copy(board(), dir.join("blinky.kicad_pcb")).unwrap();
    std::fs::write(
        dir.join("ci/power-up.toml"),
        format!(
            "name = \"power-up\"\n\
             board = \"../blinky.kicad_pcb\"\n\
             duration_ms = 5\n\
             \n\
             [[supply]]\n\
             net = \"+5V\"\n\
             kind = \"bench\"\n\
             volts = 5.0\n\
             current_limit_a = 1.0\n\
             \n\
             [[assert]]\n\
             kind = \"voltage\"\n\
             net = \"+5V\"\n\
             min = {min_v}\n"
        ),
    )
    .unwrap();
}

fn install(dir: &Path) -> Output {
    let out = Command::new(bin())
        .args(["hook", "install"])
        .current_dir(dir)
        .output()
        .expect("hook install runs");
    assert!(
        out.status.success(),
        "hook install failed: {}",
        stderr(&out)
    );
    out
}

#[test]
fn a_red_spec_blocks_the_commit_even_with_an_existing_exit_0_hook() {
    let tmp = repo();
    let dir = tmp.path();
    write_existing_hook(dir);
    write_red_spec(dir);
    let installed = install(dir);
    let said = stdout(&installed);
    assert!(
        said.contains("discovered 1 spec"),
        "install must see the spec: {said}"
    );

    git(dir, &["add", "-A"]);
    let out = commit(dir);
    let (err, log) = (stderr(&out), stdout(&out));

    assert!(
        !out.status.success(),
        "a RED spec must block the commit; git said:\n{log}\n{err}"
    );
    assert!(
        err.contains("commit blocked"),
        "the block must say why:\n{err}"
    );
    // The pre-existing hook still ran: installing a hardware gate must not
    // quietly disable the repo's other checks.
    assert!(
        dir.join("legacy-hook-ran.txt").exists(),
        "the pre-existing hook's own logic must still run"
    );
    // And nothing landed.
    let head = git(dir, &["rev-parse", "--verify", "HEAD"]);
    assert!(!head.status.success(), "no commit may exist");
}

#[test]
fn a_green_spec_lets_the_commit_through_and_still_runs_the_existing_hook() {
    let tmp = repo();
    let dir = tmp.path();
    write_existing_hook(dir);
    write_green_spec(dir);
    install(dir);

    git(dir, &["add", "-A"]);
    let out = commit(dir);
    assert!(
        out.status.success(),
        "a GREEN spec must not block the commit:\n{}\n{}",
        stdout(&out),
        stderr(&out)
    );
    assert!(
        dir.join("legacy-hook-ran.txt").exists(),
        "the pre-existing hook's own logic must still run"
    );
}

#[test]
fn the_chained_hooks_nonzero_exit_propagates() {
    let tmp = repo();
    let dir = tmp.path();
    write_rejecting_hook(dir);
    write_green_spec(dir);
    install(dir);

    git(dir, &["add", "-A"]);
    let out = commit(dir);
    let err = stderr(&out);
    assert!(
        !out.status.success(),
        "the chained hook's rejection must block the commit:\n{err}"
    );
    assert!(err.contains("legacy hook says no"), "{err}");
}

#[test]
fn a_missing_binary_blocks_the_commit_unless_the_opt_out_is_set() {
    // H8: the hook used to `exit 0` when hauksbee-ci was not on PATH, which is
    // a gate that is green forever on a fresh clone.
    let tmp = repo();
    let dir = tmp.path();
    write_red_spec(dir);
    install(dir);
    git(dir, &["add", "-A"]);

    // A PATH with nothing on it is the fresh-clone case: no hauksbee-ci
    // anywhere. git itself is invoked by absolute path so it still starts.
    let empty = tempfile::tempdir().unwrap();
    let bare = |optional: Option<&str>| {
        let mut cmd = Command::new(git_binary());
        cmd.args(["commit", "-m", "hardware change"])
            .current_dir(dir)
            .env("PATH", empty.path())
            .env_remove("GIT_DIR")
            .env_remove("GIT_INDEX_FILE");
        match optional {
            Some(v) => cmd.env("HAUKSBEE_CI_HOOK_OPTIONAL", v),
            None => cmd.env_remove("HAUKSBEE_CI_HOOK_OPTIONAL"),
        };
        cmd.output().expect("git commit runs")
    };

    let blocked = bare(None);
    let err = stderr(&blocked);
    assert!(
        !blocked.status.success(),
        "a missing binary must block, not pass: {err}"
    );
    assert!(err.contains("binary not on PATH"), "{err}");
    assert!(err.contains("HAUKSBEE_CI_HOOK_OPTIONAL"), "{err}");

    let opted_out = bare(Some("1"));
    assert!(
        opted_out.status.success(),
        "HAUKSBEE_CI_HOOK_OPTIONAL=1 must skip the check:\n{}\n{}",
        stdout(&opted_out),
        stderr(&opted_out)
    );
}

#[test]
fn the_hook_honours_hauksbee_ci_specs() {
    // H4: `init` documents HAUKSBEE_CI_SPECS as the discovery override, and the
    // generated hook must actually read it. The spec lives in `hardware/`, which
    // the default `ci:.` never looks at, so only the env var can find it.
    let tmp = repo();
    let dir = tmp.path();
    write_red_spec(dir);
    std::fs::create_dir_all(dir.join("hardware")).unwrap();
    // `board = "../blinky.kicad_pcb"` still resolves: both directories are one
    // level under the repo root.
    std::fs::rename(
        dir.join("ci/power-up.toml"),
        dir.join("hardware/power-up.toml"),
    )
    .unwrap();
    install(dir);
    git(dir, &["add", "-A"]);

    // Default discovery: the spec is invisible, so nothing gates.
    let ignored = commit_with(dir, "invisible", None);
    assert!(
        ignored.status.success(),
        "a spec outside ci:. must not be found by default:\n{}",
        stderr(&ignored)
    );

    // Now point the env var at it and change the board again: the RED spec gates.
    std::fs::write(
        dir.join("blinky.kicad_pcb"),
        std::fs::read_to_string(dir.join("blinky.kicad_pcb")).unwrap() + "\n",
    )
    .unwrap();
    git(dir, &["add", "-A"]);
    let gated = commit_with(dir, "now visible", Some("hardware"));
    let err = stderr(&gated);
    assert!(
        !gated.status.success(),
        "HAUKSBEE_CI_SPECS must make the hook find the spec:\n{err}"
    );
    assert!(err.contains("commit blocked"), "{err}");
}
