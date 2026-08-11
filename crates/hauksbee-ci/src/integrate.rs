//! Ship the repo integrations from the tool itself.
//!
//! `hauksbee-ci hook install` wires the pre-commit gate into the current
//! repository (the pre-commit framework's config when the repo uses it, a
//! plain `.git/hooks/pre-commit` otherwise), and `hauksbee-ci github-action`
//! prints (or writes) the GitHub workflow. Both are idempotent: running them
//! twice changes nothing and says so.
//!
//! The canonical integration sources live in `integrations/` at the repo
//! root; what this module emits is the minimal entry that consumes them.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};

/// The marker every artifact we write carries, so a re-run can recognise its
/// own work (and a human can see where the block came from).
const MARKER: &str = "hauksbee-ci hook install";

/// The last line of the plain-hook block. `hook uninstall` (and a refresh by
/// a newer build) removes exactly the lines between the `# {MARKER}` line and
/// this one, so a user's own hook logic around the block survives.
const END_MARKER: &str = "# end hauksbee-ci hook install";

/// Where an existing `pre-commit` hook is moved when hauksbee-ci takes the
/// filename over, and what the installed hook chains FIRST. The name is the
/// pre-commit framework's convention, so a repo that later adopts the
/// framework finds its hook already where the framework looks for it.
const LOCAL_HOOK: &str = "pre-commit.local";

/// The default `HAUKSBEE_CI_SPECS` value: colon-separated directories searched
/// for specs. `ci` then the repo root, the same default the Python shim
/// (`integrations/pre-commit`) and `.pre-commit-hooks.yaml` document, and the
/// same one [`count_discoverable_specs`] mirrors at install time.
const DEFAULT_SPEC_DIRS: &str = "ci:.";

/// Secret expression written into generated GitHub workflows. It is an input
/// reference, never the credential value itself.
const PRIVATE_TOKEN_EXPR: &str = "${{ secrets.HAUKSBEE_READ_TOKEN }}";

/// The exact string `hauksbee-ci --version` prints (name + crate version +
/// git hash). Written into the hook as the `# installed by` line AND compared
/// by the hook at run time against the live binary, so both sides of that
/// comparison come from the one function.
fn installed_by() -> String {
    format!("hauksbee-ci {}", crate::version_string())
}

/// Walk up from `start` to the repository root (the first directory that
/// contains `.git`).
pub fn find_repo_root(start: &Path) -> Option<PathBuf> {
    let mut dir = start.canonicalize().ok()?;
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Is the pre-commit gate already wired in this repo (either framework
/// config or plain hook mentioning hauksbee)?
pub fn hook_wired(root: &Path) -> bool {
    let config = root.join(".pre-commit-config.yaml");
    if let Ok(text) = fs::read_to_string(&config) {
        if text.contains("hauksbee") {
            return true;
        }
    }
    let hook = root.join(".git/hooks/pre-commit");
    matches!(fs::read_to_string(&hook), Ok(text) if text.contains("hauksbee"))
}

/// Is a GitHub workflow that runs hauksbee already present?
pub fn action_wired(root: &Path) -> bool {
    let dir = root.join(".github/workflows");
    let Ok(entries) = fs::read_dir(&dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_workflow = path.extension().is_some_and(|e| e == "yml" || e == "yaml");
        if is_workflow {
            if let Ok(text) = fs::read_to_string(&path) {
                if text.contains("hauksbee") {
                    return true;
                }
            }
        }
    }
    false
}

/// The `.pre-commit-config.yaml` repos entry for the hauksbee hooks. The two
/// hook ids are declared in `.pre-commit-hooks.yaml` of the hauksbee repo;
/// `hauksbee-ci` is the spec-driven one this tool exists for.
fn pre_commit_entry() -> String {
    format!(
        "  - repo: https://github.com/hauksbee-dev/hauksbee\n\
         \x20   rev: v{}\n\
         \x20   hooks:\n\
         \x20     - id: hauksbee-ci\n",
        env!("CARGO_PKG_VERSION")
    )
}

/// The plain `.git/hooks/pre-commit` script: run the checked-in specs when a
/// staged file could affect them. Self-contained POSIX sh, no framework.
///
/// Four things the shape of this script is load-bearing about:
///
/// * It chains `.git/hooks/pre-commit.local` FIRST and propagates its exit
///   code. Installing hauksbee-ci over an existing hook moves that hook to
///   `pre-commit.local` (the pre-commit framework's convention) rather than
///   appending to it: the canonical hook ends in `exit 0`, and an appended
///   block after it never runs, so the gate would silently never fire.
/// * A missing binary BLOCKS by default. A hook that exits 0 because the tool
///   is not installed is a gate that is green forever on a fresh clone;
///   `HAUKSBEE_CI_HOOK_OPTIONAL=1` is the explicit opt-in to skipping.
/// * Spec discovery honours `HAUKSBEE_CI_SPECS` (colon-separated directories,
///   default `ci:.`), which is what `init` and the Python shim document.
/// * The `# installed by` line records the exact build that wrote the hook, and
///   the script compares it against the live `hauksbee-ci --version` on every
///   run: a stale hook warns (one line, never blocks) with the refresh command.
///
/// Specs run one at a time so the script can count RED ones honestly; the
/// blocked-commit line reports that count and the `--no-verify` escape hatch.
fn plain_hook_script() -> String {
    let installed = installed_by();
    format!(
        "#!/bin/sh\n\
         # {MARKER}: block the commit when a staged change breaks a hauksbee-ci spec.\n\
         # installed by {installed}\n\
         # Refresh with `hauksbee-ci hook install`; remove with `hauksbee-ci hook uninstall`.\n\
         # Any hook that was here before is at {LOCAL_HOOK} and runs FIRST below.\n\
         hauksbee_hooks_dir=$(dirname \"$0\")\n\
         if [ -x \"$hauksbee_hooks_dir/{LOCAL_HOOK}\" ]; then\n\
         \x20 \"$hauksbee_hooks_dir/{LOCAL_HOOK}\" \"$@\" || exit $?\n\
         fi\n\
         if ! command -v hauksbee-ci >/dev/null 2>&1; then\n\
         \x20 if [ \"${{HAUKSBEE_CI_HOOK_OPTIONAL:-0}}\" = 1 ]; then\n\
         \x20   echo 'hauksbee-ci: binary not on PATH; HAUKSBEE_CI_HOOK_OPTIONAL=1, skipping the hardware check' >&2\n\
         \x20   exit 0\n\
         \x20 fi\n\
         \x20 echo 'hauksbee-ci: binary not on PATH, so the hardware check did NOT run; commit blocked.' >&2\n\
         \x20 echo 'hauksbee-ci: install it and re-run, or set HAUKSBEE_CI_HOOK_OPTIONAL=1 to skip the check when it is absent, or git commit --no-verify to override once.' >&2\n\
         \x20 exit 1\n\
         fi\n\
         # Warn (never block) when the binary on PATH is a different build than\n\
         # the one that wrote this hook.\n\
         installed_by='{installed}'\n\
         current=$(hauksbee-ci --version 2>/dev/null)\n\
         if [ -n \"$current\" ] && [ \"$current\" != \"$installed_by\" ]; then\n\
         \x20 echo \"hauksbee-ci: this hook was installed by '$installed_by' but the binary is '$current'; re-run: hauksbee-ci hook install\" >&2\n\
         fi\n\
         staged=$(git diff --cached --name-only --diff-filter=ACMR)\n\
         [ -z \"$staged\" ] && exit 0\n\
         case \"$staged\" in\n\
         \x20 *.kicad_pcb*|*.kicad_sch*|*.net*|*.brd*|*.d356*|*.PcbDoc*|*.board*|*.toml*|*.hex*|*.elf*)\n\
         \x20   # A hauksbee-ci spec is a TOML file with a top-level `board = ...`,\n\
         \x20   # looked for in the HAUKSBEE_CI_SPECS directories (colon-separated,\n\
         \x20   # default `ci:.`), the same contract `hauksbee-ci init` prints.\n\
         \x20   specs=''\n\
         \x20   hauksbee_spec_dirs=\"${{HAUKSBEE_CI_SPECS:-{DEFAULT_SPEC_DIRS}}}\"\n\
         \x20   while [ -n \"$hauksbee_spec_dirs\" ]; do\n\
         \x20     case \"$hauksbee_spec_dirs\" in\n\
         \x20       *:*) dir=${{hauksbee_spec_dirs%%:*}}; hauksbee_spec_dirs=${{hauksbee_spec_dirs#*:}} ;;\n\
         \x20       *)   dir=\"$hauksbee_spec_dirs\"; hauksbee_spec_dirs='' ;;\n\
         \x20     esac\n\
         \x20     [ -n \"$dir\" ] || continue\n\
         \x20     for found in $(grep -l '^board *=' \"$dir\"/*.toml 2>/dev/null || true); do\n\
         \x20       case \" $specs \" in *\" $found \"*) ;; *) specs=\"$specs $found\" ;; esac\n\
         \x20     done\n\
         \x20   done\n\
         \x20   red=0\n\
         \x20   for spec in $specs; do\n\
         \x20     hauksbee-ci run \"$spec\"\n\
         \x20     code=$?\n\
         \x20     if [ \"$code\" -eq 1 ]; then\n\
         \x20       red=$((red+1))\n\
         \x20     elif [ \"$code\" -ne 0 ]; then\n\
         \x20       exit \"$code\"\n\
         \x20     fi\n\
         \x20   done\n\
         \x20   if [ \"$red\" -gt 0 ]; then\n\
         \x20     echo \"hauksbee-ci: commit blocked: $red spec(s) RED. Fix, or git commit --no-verify to override.\" >&2\n\
         \x20     exit 1\n\
         \x20   fi\n\
         \x20   ;;\n\
         esac\n\
         exit 0\n\
         {END_MARKER}\n"
    )
}

/// The GitHub workflow YAML `github-action` prints/writes. `mode: auto` in
/// the action detects the repo's spec or board, so the generated file needs
/// no per-repo editing to start.
pub fn github_workflow_yaml() -> String {
    format!(
        "# Hardware CI: run hauksbee-ci on every change that could break the board.\n\
         # Generated by `hauksbee-ci github-action`; see the action's README for\n\
         # spec/board/matrix options (integrations/github-action in the hauksbee repo).\n\
         name: hauksbee\n\
         \n\
         # checks: write publishes the JUnit results to the Checks tab. On a fork\n\
         # PR the token is read-only; pass publish-report: false there.\n\
         permissions:\n\
         \x20 contents: read\n\
         \x20 checks: write\n\
         \n\
         on:\n\
         \x20 push:\n\
         \x20 pull_request:\n\
         \n\
         jobs:\n\
         \x20 hauksbee:\n\
         \x20   runs-on: ubuntu-latest\n\
         \x20   steps:\n\
         \x20     - uses: actions/checkout@v4\n\
         \x20     - name: Fetch the private hauksbee Action\n\
         \x20       uses: actions/checkout@v4\n\
         \x20       with:\n\
         \x20         repository: hauksbee-dev/hauksbee\n\
         \x20         ref: v{}\n\
         \x20         path: .hauksbee-action\n\
         \x20         token: {}\n\
         \x20         persist-credentials: false\n\
         \x20     - uses: ./.hauksbee-action/integrations/github-action\n\
         \x20       with:\n\
         \x20         hauksbee-token: {}\n\
         \x20         junit: hauksbee-ci-results.xml\n",
        env!("CARGO_PKG_VERSION"),
        PRIVATE_TOKEN_EXPR,
        PRIVATE_TOKEN_EXPR
    )
}

/// The directories the installed hook searches for specs, resolved against the
/// repo root: `HAUKSBEE_CI_SPECS` when set, else [`DEFAULT_SPEC_DIRS`]. The
/// hook parses the same string at run time, so install-time reporting and
/// commit-time discovery cannot disagree.
fn spec_dirs(root: &Path) -> Vec<PathBuf> {
    let configured = std::env::var("HAUKSBEE_CI_SPECS").ok();
    let raw = configured
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_SPEC_DIRS);
    let mut seen = std::collections::BTreeSet::new();
    raw.split(':')
        .filter(|d| !d.is_empty())
        .filter(|d| seen.insert(d.to_string()))
        .map(|d| root.join(d))
        .collect()
}

/// How the install output names where it looked, so the reported spec count and
/// the searched directories always come from the one source.
fn spec_dirs_phrase() -> String {
    match std::env::var("HAUKSBEE_CI_SPECS") {
        Ok(dirs) if !dirs.is_empty() => {
            format!("in {} (HAUKSBEE_CI_SPECS)", dirs.replace(':', ", "))
        }
        _ => "in ci/ and the repo root".to_string(),
    }
}

/// Count the specs the installed hook will discover, mirroring its grep
/// exactly: `*.toml` files in each [`spec_dirs`] directory whose text has a
/// top-level `board =` line. The install output reports this number so a
/// user learns "the hook found nothing to run" at install time, not at their
/// next commit.
fn count_discoverable_specs(root: &Path) -> usize {
    let mut n = 0;
    for dir in spec_dirs(root) {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() || path.extension().is_none_or(|e| e != "toml") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            // The hook's `grep '^board *='`: line-anchored `board`, optional
            // spaces, `=`.
            let is_spec = text.lines().any(|l| {
                l.strip_prefix("board")
                    .is_some_and(|rest| rest.trim_start_matches(' ').starts_with('='))
            });
            if is_spec {
                n += 1;
            }
        }
    }
    n
}

/// The lines every successful `hook install` ends with: how many specs the
/// hook will find, how to exercise it, how to bypass it once, and how to take
/// it out again.
fn install_next_steps(root: &Path) -> String {
    let n = count_discoverable_specs(root);
    let where_ = spec_dirs_phrase();
    let discovered = if n == 0 {
        format!(
            "discovered 0 specs {where_}; the hook is a no-op until one exists \
             (`hauksbee-ci init <board>` scaffolds one)"
        )
    } else {
        format!("discovered {n} spec(s) {where_}")
    };
    format!(
        "{discovered}\n\
         test it: git commit\n\
         bypass once: git commit --no-verify\n\
         remove it: hauksbee-ci hook uninstall"
    )
}

/// `hauksbee-ci hook install`: wire the pre-commit gate into the repo that
/// contains `cwd`. Detects which hook mechanism the repo uses: a
/// `.pre-commit-config.yaml` gets the framework entry, anything else gets a
/// plain `.git/hooks/pre-commit`. Idempotent both ways; a plain hook written
/// by a DIFFERENT hauksbee-ci build is refreshed in place (that is what the
/// hook's own stale-build warning tells the user to do).
pub fn hook_install(cwd: &Path) -> anyhow::Result<String> {
    let Some(root) = find_repo_root(cwd) else {
        bail!(
            "not inside a git repository (no .git found walking up from {}); \
             run this from the repo you want the hook in",
            cwd.display()
        );
    };
    let config = root.join(".pre-commit-config.yaml");
    if config.exists() {
        let text =
            fs::read_to_string(&config).with_context(|| format!("reading {}", config.display()))?;
        if text.contains("hauksbee") {
            return Ok(format!(
                "already installed: {} already references hauksbee; nothing changed",
                config.display()
            ));
        }
        // Insert the entry at the END of the top-level `repos:` list, so it
        // stays inside the list no matter what follows the list in the file and
        // runs AFTER the hooks that were already there. Appending is what a
        // human editing the file would do, and it keeps the fast formatters and
        // linters first: they finish in milliseconds, and there is no point
        // solving a circuit for a commit `black` is about to reject anyway.
        let entry = pre_commit_entry();
        let (new_text, did) = if let Some(pos) = text.lines().position(|l| l.trim_end() == "repos:")
        {
            let lines: Vec<&str> = text.lines().collect();
            let end = repos_list_end(&lines, pos);
            let mut lines = lines;
            lines.insert(end, entry.trim_end());
            let mut joined = lines.join("\n");
            joined.push('\n');
            (
                joined,
                "appended the hauksbee-ci entry to `repos:` (after your existing hooks)",
            )
        } else {
            let mut t = text.clone();
            if !t.ends_with('\n') {
                t.push('\n');
            }
            t.push_str("repos:\n");
            t.push_str(&entry);
            (t, "added a `repos:` section with the hauksbee-ci entry")
        };
        fs::write(&config, new_text).with_context(|| format!("writing {}", config.display()))?;
        return Ok(format!(
            "{did} in {}; run `pre-commit install` to activate it\n{}",
            config.display(),
            install_next_steps(&root)
        ));
    }

    // No pre-commit framework: plain git hook.
    let hooks_dir = root.join(".git/hooks");
    fs::create_dir_all(&hooks_dir).with_context(|| format!("creating {}", hooks_dir.display()))?;
    let hook = hooks_dir.join("pre-commit");
    let local = hooks_dir.join(LOCAL_HOOK);
    if hook.exists() {
        let text =
            fs::read_to_string(&hook).with_context(|| format!("reading {}", hook.display()))?;
        let script = plain_hook_script();
        if text.contains(MARKER) {
            // Exactly what this build writes: nothing to do, and in particular
            // nothing to chain a second time.
            if text == script {
                return Ok(format!(
                    "already installed: {} carries the hauksbee-ci block; nothing changed",
                    hook.display()
                ));
            }
            // Ours, but not byte-identical: either a different build wrote it
            // (the hook's own stale-build warning tells the user `hook install`
            // is the fix), or it is an older APPENDED block with the user's own
            // hook logic still around it. The second shape is the bug this
            // install path exists to prevent, so repair it the same way: the
            // user's half goes to the local hook and gets chained first.
            let Some(remainder) = strip_hook_block(&text) else {
                bail!(
                    "{} carries a hauksbee-ci block this build cannot safely \
                     replace (no `{END_MARKER}` line); edit the file by hand",
                    hook.display()
                );
            };
            let has_own_logic = !remainder.trim().is_empty() && remainder.trim() != "#!/bin/sh";
            if has_own_logic {
                park_local_hook(&local, &remainder)?;
            }
            fs::write(&hook, &script).with_context(|| format!("writing {}", hook.display()))?;
            set_executable(&hook)?;
            let note = if has_own_logic {
                format!(
                    "refreshed {} and moved your own hook logic to {}, which now runs \
                     FIRST (an appended block after your hook's `exit 0` never ran)",
                    hook.display(),
                    local.display()
                )
            } else {
                format!(
                    "refreshed the hauksbee-ci block in {} (a different hauksbee-ci build wrote it)",
                    hook.display()
                )
            };
            return Ok(format!("{note}\n{}", install_next_steps(&root)));
        }
        // Someone else's hook. Appending to it does not work: the canonical
        // hook shape ends in `exit 0`, so an appended block never runs and the
        // gate would report success while gating nothing. Move it aside and
        // chain it first instead, the pre-commit framework's pattern.
        park_local_hook(&local, &text)?;
        fs::write(&hook, &script).with_context(|| format!("writing {}", hook.display()))?;
        set_executable(&hook)?;
        return Ok(format!(
            "moved your existing hook to {} and installed {}, which runs the moved \
             hook FIRST and blocks the commit if it fails\n{}",
            local.display(),
            hook.display(),
            install_next_steps(&root)
        ));
    }
    fs::write(&hook, plain_hook_script()).with_context(|| format!("writing {}", hook.display()))?;
    set_executable(&hook)?;
    Ok(format!(
        "installed {}\n{}",
        hook.display(),
        install_next_steps(&root)
    ))
}

/// Index just past the last line of the `repos:` list that begins at
/// `repos_line`, for inserting a new entry at the end of the list. The list
/// runs while lines are indented (its items and their continuations) or blank;
/// it ends at the first line at column 0 that is not part of it, or at EOF.
/// Trailing blank lines belong after the list, not inside it.
fn repos_list_end(lines: &[&str], repos_line: usize) -> usize {
    let mut end = repos_line + 1;
    let mut last_content = end;
    while end < lines.len() {
        let line = lines[end];
        if line.trim().is_empty() {
            end += 1;
            continue;
        }
        let indented = line.starts_with(' ') || line.starts_with('\t');
        if !indented {
            break;
        }
        end += 1;
        last_content = end;
    }
    last_content
}

/// Move an existing hook out of the way so hauksbee-ci can own the `pre-commit`
/// filename and chain the old hook first. Refuses when the destination is
/// already taken rather than overwriting whatever is there.
///
/// The parked file is invoked directly by the installed hook, so it needs a
/// shebang even if git was happy to run it without one.
fn park_local_hook(local: &Path, text: &str) -> anyhow::Result<()> {
    if local.exists() {
        bail!(
            "{} already exists, so the hook currently in place cannot be moved \
             aside without losing one of the two; merge them by hand (or delete \
             the stale one) and re-run",
            local.display()
        );
    }
    let mut parked = String::new();
    if !text.starts_with("#!") {
        parked.push_str("#!/bin/sh\n");
    }
    parked.push_str(text);
    if !parked.ends_with('\n') {
        parked.push('\n');
    }
    fs::write(local, parked).with_context(|| format!("writing {}", local.display()))?;
    set_executable(local)?;
    Ok(())
}

/// Remove the hauksbee-ci block (the lines from `# {MARKER}` through
/// [`END_MARKER`], inclusive) from hook text. `None` when the block's bounds
/// cannot be found, in which case nothing must be deleted.
fn strip_hook_block(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let begin_prefix = format!("# {MARKER}");
    let begin = lines
        .iter()
        .position(|l| l.trim_start().starts_with(&begin_prefix))?;
    let end = lines.iter().position(|l| l.trim() == END_MARKER)?;
    if end < begin {
        return None;
    }
    let mut kept: Vec<&str> = Vec::new();
    kept.extend(&lines[..begin]);
    kept.extend(&lines[end + 1..]);
    while kept.last().is_some_and(|l| l.trim().is_empty()) {
        kept.pop();
    }
    if kept.is_empty() {
        return Some(String::new());
    }
    Some(kept.join("\n") + "\n")
}

/// `hauksbee-ci hook uninstall`: undo whichever wiring [`hook_install`] did in
/// this repo. Removes the hauksbee-ci block from the plain
/// `.git/hooks/pre-commit` (deleting the file when the block was all there
/// was), or removes the hauksbee entry from `.pre-commit-config.yaml`.
/// Refuses to touch a hook hauksbee-ci did not write.
pub fn hook_uninstall(cwd: &Path) -> anyhow::Result<String> {
    let Some(root) = find_repo_root(cwd) else {
        bail!(
            "not inside a git repository (no .git found walking up from {}); \
             run this from the repo the hook is in",
            cwd.display()
        );
    };

    // Framework flavor first, mirroring install's detection order.
    let config = root.join(".pre-commit-config.yaml");
    if let Ok(text) = fs::read_to_string(&config) {
        if text.contains("hauksbee") {
            let new_text = remove_pre_commit_entry(&text);
            fs::write(&config, new_text)
                .with_context(|| format!("writing {}", config.display()))?;
            return Ok(format!(
                "removed the hauksbee-ci entry from {}; run `pre-commit install` \
                 to refresh the installed hooks",
                config.display()
            ));
        }
    }

    let hooks_dir = root.join(".git/hooks");
    let hook = hooks_dir.join("pre-commit");
    let local = hooks_dir.join(LOCAL_HOOK);
    let Ok(text) = fs::read_to_string(&hook) else {
        return Ok(format!(
            "nothing to uninstall: no hauksbee entry in .pre-commit-config.yaml \
             and no {}",
            hook.display()
        ));
    };
    if !text.contains(MARKER) {
        bail!(
            "{} was not installed by hauksbee-ci; refusing to touch it",
            hook.display()
        );
    }
    // Install moved a pre-existing hook to the local file and chained it; put
    // it back where git looks for it, so uninstalling leaves the repo exactly
    // as install found it.
    if local.exists() {
        let parked =
            fs::read_to_string(&local).with_context(|| format!("reading {}", local.display()))?;
        fs::write(&hook, parked).with_context(|| format!("writing {}", hook.display()))?;
        set_executable(&hook)?;
        fs::remove_file(&local).with_context(|| format!("removing {}", local.display()))?;
        return Ok(format!(
            "removed the hauksbee-ci hook and restored your own hook from {} back to {}",
            local.display(),
            hook.display()
        ));
    }
    let Some(remainder) = strip_hook_block(&text) else {
        bail!(
            "{} carries a hauksbee-ci marker but not a complete block (no \
             `{END_MARKER}` line); edit the file by hand",
            hook.display()
        );
    };
    if remainder.trim().is_empty() || remainder.trim() == "#!/bin/sh" {
        fs::remove_file(&hook).with_context(|| format!("removing {}", hook.display()))?;
        return Ok(format!("removed {}", hook.display()));
    }
    fs::write(&hook, remainder).with_context(|| format!("writing {}", hook.display()))?;
    Ok(format!(
        "removed the hauksbee-ci block from {}; the rest of your hook is untouched",
        hook.display()
    ))
}

/// Remove the hauksbee repos entry from `.pre-commit-config.yaml` text: the
/// `- repo: ...hauksbee` line plus its indented continuation lines, up to the
/// next list item or dedent. Structural rather than line-count-based, so a
/// user-edited `rev:` still comes out cleanly.
fn remove_pre_commit_entry(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let Some(start) = lines.iter().position(|l| {
        let t = l.trim_start();
        t.starts_with("- repo:") && t.contains("hauksbee")
    }) else {
        return text.to_string();
    };
    let indent = lines[start].len() - lines[start].trim_start().len();
    let mut end = start + 1;
    while end < lines.len() {
        let line = lines[end];
        if line.trim().is_empty() {
            end += 1;
            continue;
        }
        let line_indent = line.len() - line.trim_start().len();
        // The entry ends at the next sibling list item or anything dedented
        // to (or past) the entry's own level.
        if line_indent <= indent && line.trim_start().starts_with("- ") {
            break;
        }
        if line_indent < indent || (line_indent == indent && !line.trim_start().starts_with("- ")) {
            break;
        }
        end += 1;
    }
    let mut kept: Vec<&str> = Vec::new();
    kept.extend(&lines[..start]);
    kept.extend(&lines[end..]);
    while kept.last().is_some_and(|l| l.trim().is_empty()) {
        kept.pop();
    }
    if kept.is_empty() {
        return String::new();
    }
    kept.join("\n") + "\n"
}

/// CLI entry for `hauksbee-ci hook uninstall`, kept here so main.rs stays a
/// pure dispatch table. Exit 0 with the outcome, 2 on error, matching
/// `hook install`'s contract.
pub fn run_hook_uninstall() -> std::process::ExitCode {
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("hauksbee-ci: cannot determine the current directory: {e}");
            return std::process::ExitCode::from(2);
        }
    };
    match hook_uninstall(&cwd) {
        Ok(msg) => {
            println!("{msg}");
            std::process::ExitCode::from(0)
        }
        Err(e) => {
            eprintln!("hauksbee-ci: {e}");
            std::process::ExitCode::from(2)
        }
    }
}

#[cfg(unix)]
fn set_executable(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(perms.mode() | 0o111);
    fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

/// `hauksbee-ci github-action --write <path>`: write the workflow file into the
/// repository containing `cwd`, on the same terms `hook install` uses.
///
/// A RELATIVE path resolves against the repo ROOT, not the current directory:
/// GitHub only reads `.github/workflows` at the top of the repo, so the default
/// `--write` path written into a subdirectory would be a workflow that silently
/// never runs. Outside a repo it refuses, exactly as `hook install` does, rather
/// than dropping a workflow into whatever directory the user happened to be in.
/// Idempotent: an identical existing file is a no-op; a different one is
/// refused rather than clobbered.
pub fn github_action_write(cwd: &Path, path: &Path) -> anyhow::Result<String> {
    let Some(root) = find_repo_root(cwd) else {
        bail!(
            "not inside a git repository (no .git found walking up from {}); \
             a GitHub workflow only does anything inside one, so run this from \
             the repo you want it in (or pass an absolute --write path)",
            cwd.display()
        );
    };
    let path = &if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let yaml = github_workflow_yaml();
    if let Ok(existing) = fs::read_to_string(path) {
        if existing == yaml {
            return Ok(format!("already up to date: {}", path.display()));
        }
        bail!(
            "{} exists with different content; not overwriting. Remove it (or \
             pick another --write path) and re-run, or merge by hand from \
             `hauksbee-ci github-action` on stdout",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
    }
    fs::write(path, yaml).with_context(|| format!("writing {}", path.display()))?;
    Ok(format!(
        "wrote {path}\n\
         next: commit and push it: git add {path} && git commit -m \"add hauksbee \
         hardware CI\" && git push",
        path = path.display()
    ))
}

/// The one next-step line a GREEN run ends with: point at whichever repo
/// wiring is missing, and stay silent when both the hook and the workflow
/// are already in place (or when there is no repo to wire).
pub fn green_next_step(cwd: &Path) -> Option<String> {
    let root = find_repo_root(cwd)?;
    let hook = hook_wired(&root);
    let action = action_wired(&root);
    match (hook, action) {
        (true, true) => None,
        (false, true) => {
            Some("next: gate commits locally too: `hauksbee-ci hook install`".to_string())
        }
        (true, false) => Some(
            "next: gate pushes and PRs: `hauksbee-ci github-action --write` \
             writes .github/workflows/hauksbee.yml"
                .to_string(),
        ),
        (false, false) => Some(
            "next: wire this into your repo: `hauksbee-ci hook install` (pre-commit \
             gate) and `hauksbee-ci github-action --write` (GitHub workflow)"
                .to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git_repo(dir: &Path) {
        assert!(Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir)
            .status()
            .expect("git init")
            .success());
    }

    #[test]
    fn plain_hook_install_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        git_repo(tmp.path());
        let first = hook_install(tmp.path()).unwrap();
        assert!(first.starts_with("installed"), "{first}");
        let hook = tmp.path().join(".git/hooks/pre-commit");
        let written = fs::read_to_string(&hook).unwrap();
        assert!(written.contains(MARKER));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_ne!(fs::metadata(&hook).unwrap().permissions().mode() & 0o111, 0);
        }
        let second = hook_install(tmp.path()).unwrap();
        assert!(second.starts_with("already installed"), "{second}");
        assert_eq!(fs::read_to_string(&hook).unwrap(), written);
    }

    #[test]
    fn an_existing_hook_is_moved_aside_and_chained_first() {
        // H1: appending after an existing hook put the hauksbee block AFTER that
        // hook's `exit 0`, so the gate never ran while install reported success.
        // The existing hook must move to pre-commit.local and be chained FIRST.
        let tmp = tempfile::tempdir().unwrap();
        git_repo(tmp.path());
        let hook = tmp.path().join(".git/hooks/pre-commit");
        let local = tmp.path().join(".git/hooks/pre-commit.local");
        fs::write(&hook, "#!/bin/sh\necho preexisting\nexit 0\n").unwrap();
        let msg = hook_install(tmp.path()).unwrap();
        assert!(msg.contains("pre-commit.local"), "{msg}");
        let text = fs::read_to_string(&hook).unwrap();
        assert!(text.contains(MARKER), "{text}");
        assert!(
            !text.contains("echo preexisting"),
            "the old hook must move out, not stay in the file: {text}"
        );
        assert!(
            fs::read_to_string(&local)
                .unwrap()
                .contains("echo preexisting"),
            "the old hook must be parked in pre-commit.local"
        );
        // The chain runs the local hook BEFORE any hauksbee logic, and
        // propagates its exit code.
        let chain = text
            .find("pre-commit.local")
            .expect("chains the local hook");
        let gate = text.find("hauksbee-ci run").expect("runs specs");
        assert!(chain < gate, "the local hook must run first:\n{text}");
        assert!(text.contains("|| exit $?"), "{text}");
        // Idempotent: a second install must not chain the local hook twice.
        let again = hook_install(tmp.path()).unwrap();
        assert!(again.starts_with("already installed"), "{again}");
        let text2 = fs::read_to_string(&hook).unwrap();
        assert_eq!(text, text2);
        assert_eq!(
            text2.matches("pre-commit.local\" \"$@\"").count(),
            1,
            "{text2}"
        );
    }

    #[test]
    fn install_repairs_a_legacy_appended_block() {
        // The pre-H1 shape on disk: the user's hook, then our block after it.
        // Re-installing must move the user's half to the local hook and leave a
        // hook whose gate is actually reachable.
        let tmp = tempfile::tempdir().unwrap();
        git_repo(tmp.path());
        let hook = tmp.path().join(".git/hooks/pre-commit");
        let body = plain_hook_script().replace("#!/bin/sh\n", "");
        fs::write(&hook, format!("#!/bin/sh\necho mine\nexit 0\n\n{body}")).unwrap();
        let msg = hook_install(tmp.path()).unwrap();
        assert!(msg.contains("never ran"), "{msg}");
        let text = fs::read_to_string(&hook).unwrap();
        assert_eq!(text, plain_hook_script());
        assert!(
            fs::read_to_string(tmp.path().join(".git/hooks/pre-commit.local"))
                .unwrap()
                .contains("echo mine")
        );
    }

    #[test]
    fn install_refuses_when_the_local_hook_slot_is_taken() {
        let tmp = tempfile::tempdir().unwrap();
        git_repo(tmp.path());
        fs::write(
            tmp.path().join(".git/hooks/pre-commit"),
            "#!/bin/sh\necho preexisting\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join(".git/hooks/pre-commit.local"),
            "#!/bin/sh\necho something else\n",
        )
        .unwrap();
        let err = hook_install(tmp.path()).unwrap_err().to_string();
        assert!(err.contains("already exists"), "{err}");
        // Neither file was touched.
        assert!(fs::read_to_string(tmp.path().join(".git/hooks/pre-commit"))
            .unwrap()
            .contains("echo preexisting"));
    }

    #[test]
    fn uninstall_restores_the_hook_install_moved_aside() {
        let tmp = tempfile::tempdir().unwrap();
        git_repo(tmp.path());
        let hook = tmp.path().join(".git/hooks/pre-commit");
        let original = "#!/bin/sh\necho preexisting\nexit 0\n";
        fs::write(&hook, original).unwrap();
        hook_install(tmp.path()).unwrap();
        let msg = hook_uninstall(tmp.path()).unwrap();
        assert!(msg.contains("restored"), "{msg}");
        assert_eq!(fs::read_to_string(&hook).unwrap(), original);
        assert!(!tmp.path().join(".git/hooks/pre-commit.local").exists());
    }

    #[test]
    fn the_hook_reads_the_documented_spec_directories() {
        // H4: `init` tells users HAUKSBEE_CI_SPECS overrides discovery, but the
        // generated hook hardcoded `ci/*.toml *.toml`.
        let tmp = tempfile::tempdir().unwrap();
        git_repo(tmp.path());
        hook_install(tmp.path()).unwrap();
        let text = fs::read_to_string(tmp.path().join(".git/hooks/pre-commit")).unwrap();
        assert!(text.contains("HAUKSBEE_CI_SPECS"), "{text}");
        assert!(text.contains("ci:."), "the documented default: {text}");
        assert!(
            !text.contains("ci/*.toml *.toml"),
            "the hardcoded discovery must be gone: {text}"
        );
    }

    #[test]
    fn a_missing_binary_blocks_unless_the_opt_out_is_set() {
        // H8: a hook that exits 0 because the tool is not installed is a gate
        // that is green forever on a fresh clone.
        let tmp = tempfile::tempdir().unwrap();
        git_repo(tmp.path());
        hook_install(tmp.path()).unwrap();
        let text = fs::read_to_string(tmp.path().join(".git/hooks/pre-commit")).unwrap();
        assert!(text.contains("HAUKSBEE_CI_HOOK_OPTIONAL"), "{text}");
        assert!(text.contains("commit blocked"), "{text}");
        assert!(
            !text.contains("skipping hardware check' >&2\n  exit 0"),
            "an unconditional skip must be gone: {text}"
        );
    }

    #[test]
    fn pre_commit_config_gets_the_entry_under_repos() {
        let tmp = tempfile::tempdir().unwrap();
        git_repo(tmp.path());
        fs::write(
            tmp.path().join(".pre-commit-config.yaml"),
            "repos:\n  - repo: https://github.com/psf/black\n    rev: 24.1.0\n    hooks:\n      - id: black\n",
        )
        .unwrap();
        let msg = hook_install(tmp.path()).unwrap();
        assert!(msg.contains("pre-commit install"), "{msg}");
        let text = fs::read_to_string(tmp.path().join(".pre-commit-config.yaml")).unwrap();
        let repos_line = text.lines().position(|l| l == "repos:").unwrap();
        let black_line = text.lines().position(|l| l.contains("psf/black")).unwrap();
        let hauksbee_line = text
            .lines()
            .position(|l| l.contains("hauksbee-dev/hauksbee"))
            .unwrap();
        // L11: appended at the end of the list, not prepended: the fast
        // formatters keep running first.
        assert!(repos_line < black_line, "{text}");
        assert!(
            black_line < hauksbee_line,
            "the entry must be appended after the existing hooks:\n{text}"
        );
        assert!(text.contains("id: hauksbee-ci"));
        assert!(text.contains("id: black"));
        // Idempotent.
        let again = hook_install(tmp.path()).unwrap();
        assert!(again.starts_with("already installed"), "{again}");
    }

    #[test]
    fn workflow_write_is_idempotent_and_refuses_divergence() {
        let tmp = tempfile::tempdir().unwrap();
        git_repo(tmp.path());
        let path = tmp.path().join(".github/workflows/hauksbee.yml");
        let first = github_action_write(tmp.path(), &path).unwrap();
        assert!(first.starts_with("wrote"), "{first}");
        let second = github_action_write(tmp.path(), &path).unwrap();
        assert!(second.starts_with("already up to date"), "{second}");
        fs::write(&path, "something else\n").unwrap();
        let err = github_action_write(tmp.path(), &path)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not overwriting"), "{err}");
    }

    #[test]
    fn generated_workflow_authenticates_private_action_and_repository() {
        let yaml = github_workflow_yaml();
        assert!(yaml.contains("repository: hauksbee-dev/hauksbee"), "{yaml}");
        assert!(yaml.contains("path: .hauksbee-action"), "{yaml}");
        assert!(
            yaml.contains("token: ${{ secrets.HAUKSBEE_READ_TOKEN }}"),
            "{yaml}"
        );
        assert!(yaml.contains("persist-credentials: false"), "{yaml}");
        assert!(
            yaml.contains("uses: ./.hauksbee-action/integrations/github-action"),
            "{yaml}"
        );
        assert!(
            yaml.contains("hauksbee-token: ${{ secrets.HAUKSBEE_READ_TOKEN }}"),
            "{yaml}"
        );
        assert!(!yaml.contains("uses: hauksbee-dev/hauksbee/integrations/github-action@"));
    }

    #[test]
    fn workflow_write_refuses_outside_a_repo_like_hook_install_does() {
        // L3: `hook install` refused outside a git repo while `github-action
        // --write` quietly wrote into the current directory.
        let tmp = tempfile::tempdir().unwrap();
        let err = github_action_write(tmp.path(), Path::new(".github/workflows/hauksbee.yml"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not inside a git repository"), "{err}");
        assert!(!tmp.path().join(".github").exists(), "nothing was written");
    }

    #[test]
    fn a_relative_write_path_lands_at_the_repo_root() {
        // GitHub only reads .github/workflows at the top of the repo, so a
        // relative --write from a subdirectory must not write a workflow there.
        let tmp = tempfile::tempdir().unwrap();
        git_repo(tmp.path());
        let sub = tmp.path().join("hardware/ci");
        fs::create_dir_all(&sub).unwrap();
        let msg = github_action_write(&sub, Path::new(".github/workflows/hauksbee.yml")).unwrap();
        assert!(msg.starts_with("wrote"), "{msg}");
        assert!(tmp.path().join(".github/workflows/hauksbee.yml").exists());
        assert!(!sub.join(".github").exists(), "not in the subdirectory");
    }

    #[test]
    fn install_output_names_specs_test_bypass_and_uninstall() {
        let tmp = tempfile::tempdir().unwrap();
        git_repo(tmp.path());
        // One discoverable spec in ci/, one root TOML that is NOT a spec.
        fs::create_dir(tmp.path().join("ci")).unwrap();
        fs::write(
            tmp.path().join("ci/power-up.toml"),
            "board = \"../hw/board.kicad_pcb\"\n",
        )
        .unwrap();
        fs::write(tmp.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        let msg = hook_install(tmp.path()).unwrap();
        assert!(
            msg.contains("discovered 1 spec(s) in ci/ and the repo root"),
            "{msg}"
        );
        assert!(msg.contains("test it: git commit"), "{msg}");
        assert!(msg.contains("bypass once: git commit --no-verify"), "{msg}");
        assert!(msg.contains("hauksbee-ci hook uninstall"), "{msg}");
    }

    #[test]
    fn install_output_warns_when_no_specs_are_discoverable() {
        let tmp = tempfile::tempdir().unwrap();
        git_repo(tmp.path());
        let msg = hook_install(tmp.path()).unwrap();
        assert!(msg.contains("discovered 0 specs"), "{msg}");
        assert!(msg.contains("hauksbee-ci init"), "{msg}");
    }

    #[test]
    fn hook_records_the_build_and_carries_the_exact_red_line() {
        let tmp = tempfile::tempdir().unwrap();
        git_repo(tmp.path());
        hook_install(tmp.path()).unwrap();
        let text = fs::read_to_string(tmp.path().join(".git/hooks/pre-commit")).unwrap();
        // U6: the installing build's identity, in the comment and in the
        // runtime comparison, both matching `hauksbee-ci --version` output.
        let installed = format!("hauksbee-ci {}", crate::version_string());
        assert!(
            text.contains(&format!("# installed by {installed}")),
            "{text}"
        );
        assert!(
            text.contains(&format!("installed_by='{installed}'")),
            "{text}"
        );
        assert!(text.contains("hauksbee-ci --version"), "{text}");
        assert!(text.contains("re-run: hauksbee-ci hook install"), "{text}");
        // U8: the blocked-commit wording, byte for byte around the count.
        assert!(
            text.contains(
                "hauksbee-ci: commit blocked: $red spec(s) RED. Fix, or git commit --no-verify to override."
            ),
            "{text}"
        );
        assert!(text.contains(END_MARKER), "{text}");
    }

    #[test]
    fn install_refreshes_a_block_from_a_different_build() {
        let tmp = tempfile::tempdir().unwrap();
        git_repo(tmp.path());
        hook_install(tmp.path()).unwrap();
        let hook = tmp.path().join(".git/hooks/pre-commit");
        // Simulate a hook written by an older build.
        let stale = fs::read_to_string(&hook).unwrap().replace(
            &format!("installed_by='{}'", installed_by()),
            "installed_by='hauksbee-ci 0.0.0 (git dead)'",
        );
        fs::write(&hook, stale).unwrap();
        let msg = hook_install(tmp.path()).unwrap();
        assert!(msg.starts_with("refreshed"), "{msg}");
        let text = fs::read_to_string(&hook).unwrap();
        assert!(
            text.contains(&format!("installed_by='{}'", installed_by())),
            "{text}"
        );
        assert!(!text.contains("0.0.0 (git dead)"), "{text}");
    }

    #[test]
    fn uninstall_removes_a_hook_that_is_entirely_ours() {
        let tmp = tempfile::tempdir().unwrap();
        git_repo(tmp.path());
        hook_install(tmp.path()).unwrap();
        let msg = hook_uninstall(tmp.path()).unwrap();
        assert!(msg.starts_with("removed"), "{msg}");
        assert!(!tmp.path().join(".git/hooks/pre-commit").exists());
        // Uninstalling again reports nothing to do, not an error.
        let again = hook_uninstall(tmp.path()).unwrap();
        assert!(again.starts_with("nothing to uninstall"), "{again}");
    }

    #[test]
    fn uninstall_strips_only_our_block_from_a_shared_hook() {
        // A hand-edited hook that wraps our block (nobody writes this shape now,
        // but a user can): uninstall removes the block and leaves the rest.
        let tmp = tempfile::tempdir().unwrap();
        git_repo(tmp.path());
        let hook = tmp.path().join(".git/hooks/pre-commit");
        let body = plain_hook_script().replace("#!/bin/sh\n", "");
        fs::write(&hook, format!("#!/bin/sh\n{body}\necho after\n")).unwrap();
        let msg = hook_uninstall(tmp.path()).unwrap();
        assert!(msg.contains("rest of your hook is untouched"), "{msg}");
        let text = fs::read_to_string(&hook).unwrap();
        assert!(text.contains("echo after"), "{text}");
        assert!(!text.contains(MARKER), "{text}");
    }

    #[test]
    fn uninstall_refuses_a_hook_we_did_not_write() {
        let tmp = tempfile::tempdir().unwrap();
        git_repo(tmp.path());
        let hook = tmp.path().join(".git/hooks/pre-commit");
        fs::write(&hook, "#!/bin/sh\necho someone else\n").unwrap();
        let err = hook_uninstall(tmp.path()).unwrap_err().to_string();
        assert!(err.contains("refusing"), "{err}");
        assert!(fs::read_to_string(&hook).unwrap().contains("someone else"));
    }

    #[test]
    fn uninstall_removes_only_the_hauksbee_entry_from_pre_commit_config() {
        let tmp = tempfile::tempdir().unwrap();
        git_repo(tmp.path());
        let config = tmp.path().join(".pre-commit-config.yaml");
        fs::write(
            &config,
            "repos:\n  - repo: https://github.com/psf/black\n    rev: 24.1.0\n    hooks:\n      - id: black\n",
        )
        .unwrap();
        let installed = hook_install(tmp.path()).unwrap();
        assert!(installed.contains("appended"), "{installed}");
        let msg = hook_uninstall(tmp.path()).unwrap();
        assert!(msg.contains("removed the hauksbee-ci entry"), "{msg}");
        let text = fs::read_to_string(&config).unwrap();
        assert!(!text.contains("hauksbee"), "{text}");
        assert!(text.contains("id: black"), "{text}");
        assert!(text.contains("repos:"), "{text}");
    }

    #[test]
    fn hook_script_is_valid_posix_sh() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("hook.sh");
        fs::write(&path, plain_hook_script()).unwrap();
        let status = Command::new("sh")
            .arg("-n")
            .arg(&path)
            .status()
            .expect("sh -n");
        assert!(status.success(), "sh -n rejected the generated hook script");
    }

    #[test]
    fn github_action_write_prints_the_commit_and_push_step() {
        let tmp = tempfile::tempdir().unwrap();
        git_repo(tmp.path());
        let path = tmp.path().join(".github/workflows/hauksbee.yml");
        let msg = github_action_write(tmp.path(), &path).unwrap();
        assert!(msg.contains("git add"), "{msg}");
        assert!(msg.contains("git push"), "{msg}");
    }

    #[test]
    fn green_next_step_names_only_the_missing_wiring() {
        let tmp = tempfile::tempdir().unwrap();
        git_repo(tmp.path());
        let both = green_next_step(tmp.path()).unwrap();
        assert!(both.contains("hook install") && both.contains("github-action"));
        hook_install(tmp.path()).unwrap();
        let action_only = green_next_step(tmp.path()).unwrap();
        assert!(action_only.contains("github-action") && !action_only.contains("hook install"));
        github_action_write(
            tmp.path(),
            &tmp.path().join(".github/workflows/hauksbee.yml"),
        )
        .unwrap();
        assert_eq!(green_next_step(tmp.path()), None);
    }
}
