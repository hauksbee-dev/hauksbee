//! The extraction's instruction must not ride on a trailing positional argument.
//!
//! codex's `--image` takes many values. An extraction passes one `--image` per
//! rendered datasheet page, so `--image p1 --image p2 ... "<prompt>"` parsed the
//! prompt as one more image path, and codex then reported "No prompt provided
//! via stdin" and exited 1. Every codex extraction failed this way, and because
//! codex's stderr went to /dev/null the whole thing surfaced as
//! `codex exited with status 1: ` with nothing after the colon.
//!
//! The prompt goes in on stdin now, which no future variadic flag can swallow.
//! This test reads the source, because the alternative is spending a real codex
//! run (and real money) on every CI build to find out.

use std::path::Path;

fn source() -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/datasheet.rs");
    std::fs::read_to_string(p).expect("read datasheet.rs")
}

#[test]
fn the_instruction_is_written_to_stdin() {
    let s = source();
    let stdin_block = s
        .split("child.stdin.take()")
        .nth(1)
        .expect("the spawn path writes to the child's stdin");
    assert!(
        stdin_block.contains("Read prompt.md"),
        "the instruction must be delivered on stdin, where no variadic flag can \
         eat it; found this after the stdin take:\n{}",
        &stdin_block[..stdin_block.len().min(400)]
    );
}

#[test]
fn the_instruction_is_not_also_a_trailing_argument() {
    // Belt and braces: if someone re-adds the positional form later, the two
    // copies disagree and codex sees the prompt twice or not at all.
    let s = source();
    let spawn = s
        .split("fn run_codex_once")
        .nth(1)
        .expect("run_codex_once exists");
    let spawn = &spawn[..spawn.find("\nfn ").unwrap_or(spawn.len())];
    assert!(
        !spawn.contains(".arg(\n            \"Read prompt.md"),
        "the prompt must not be passed as a positional argument as well"
    );
}

#[test]
fn codex_stderr_is_captured_not_discarded() {
    // The reason this bug took a real run to find: the only explanation codex
    // gave went to /dev/null.
    let s = source();
    let spawn = s
        .split("fn run_codex_once")
        .nth(1)
        .expect("run_codex_once exists");
    let spawn = &spawn[..spawn.find("\nfn ").unwrap_or(spawn.len())];
    assert!(
        !spawn.contains("stderr(Stdio::null())"),
        "codex's stderr carries the reason it failed; discarding it turns every \
         failure into `exited with status 1: `"
    );
    assert!(
        spawn.contains("codex-stderr.log"),
        "it should land in the sandbox so the error can quote its tail"
    );
}

#[test]
fn the_model_is_chosen_explicitly() {
    let s = source();
    let spawn = s
        .split("fn run_codex_once")
        .nth(1)
        .expect("run_codex_once exists");
    let spawn = &spawn[..spawn.find("\nfn ").unwrap_or(spawn.len())];
    assert!(
        spawn.contains("\"--model\""),
        "the run must name its model rather than inherit whatever the user's \
         codex defaults to, which decides how good the extraction is"
    );
    assert!(
        spawn.contains("model_reasoning_effort"),
        "and set the reasoning effort with it"
    );
}
