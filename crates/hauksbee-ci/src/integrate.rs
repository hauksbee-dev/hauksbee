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
         staged_lower=$(printf '%s' \"$staged\" | tr '[:upper:]' '[:lower:]')\n\
         case \"$staged_lower\" in\n\
         \x20 *.kicad_pcb*|*.kicad_sch*|*.net*|*.brd*|*.d356*|*.pcbdoc*|*.board*|*.xml*|*.zip*|*.tgz*|*.tar.gz*|*.tar*|*.toml*|*.hex*|*.elf*)\n\
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
/// A build without a verified Hauksbee identity errors rather than emitting a
/// credential-bearing workflow pinned to zeros or to a foreign repository; the
/// CLI turns that into a normal exit-2 diagnostic.
pub fn try_github_workflow_yaml() -> anyhow::Result<String> {
    #[cfg(test)]
    let source_commit =
        option_env!("GIT_HASH").or(Some("0123456789abcdef0123456789abcdef01234567"));
    #[cfg(not(test))]
    let source_commit = option_env!("GIT_HASH");
    #[cfg(test)]
    let release_tag = None;
    #[cfg(not(test))]
    let release_tag = option_env!("GIT_TAG");
    github_workflow_yaml_for(source_commit, release_tag)
}

fn github_workflow_yaml_for(
    source_commit: Option<&str>,
    release_tag: Option<&str>,
) -> anyhow::Result<String> {
    // The pinned Action reference must name a real Hauksbee object.
    // Source archives intentionally carry no identity; refuse generation
    // instead of emitting zeros or borrowing an enclosing consumer repo HEAD.
    let release_commit = source_commit
        .filter(|hash| hash.len() == 40 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
        .context(
            "this hauksbee-ci build has no verified Hauksbee source commit; rebuild from the Hauksbee Git root or set HAUKSBEE_SOURCE_COMMIT",
        )?;
    let acquisition = match release_tag {
        Some(tag)
            if tag == format!("v{}", env!("CARGO_PKG_VERSION"))
                && tag
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)) =>
        {
            format!(
                "         \x20hauksbee-version: {tag}\n\
                 \x20         prefer-prebuilt: true\n"
            )
        }
        _ => "         \x20         # This clean commit is not an exact release tag.\n\
              \x20         # Build the pinned source instead of guessing a release.\n\
              \x20         prefer-prebuilt: false\n"
            .to_string(),
    };
    Ok(format!(
        "# Hardware CI: run hauksbee-ci on every change that could break the board.\n\
         # Generated by `hauksbee-ci github-action`; see the action's README for\n\
         # spec/board/matrix options (integrations/github-action in the hauksbee repo).\n\
         name: hauksbee\n\
         \n\
         # checks: write publishes the JUnit results to the Checks tab. Fork PRs\n\
         # run without checks: write, so publish-report below gates the report\n\
         # step to same-repository events; the hardware check itself still runs.\n\
         permissions:\n\
         \x20 contents: read\n\
         \x20 checks: write\n\
         \n\
         on:\n\
         \x20 push:\n\
         \x20   paths: [\"ci/**\", \"*.toml\", \".github/workflows/**\", \"hardware/**\", \"firmware/**\", \"models/**\", \"**/*.kicad_pcb\", \"**/*.kicad_sch\", \"**/*.net\", \"**/*.brd\", \"**/*.PcbDoc\", \"**/*.d356\", \"**/*.board\", \"**/*.xml\", \"**/*.zip\", \"**/*.tgz\", \"**/*.tar.gz\", \"**/*.tar\"]\n\
         \x20 pull_request:\n\
         \x20   paths: [\"ci/**\", \"*.toml\", \".github/workflows/**\", \"hardware/**\", \"firmware/**\", \"models/**\", \"**/*.kicad_pcb\", \"**/*.kicad_sch\", \"**/*.net\", \"**/*.brd\", \"**/*.PcbDoc\", \"**/*.d356\", \"**/*.board\", \"**/*.xml\", \"**/*.zip\", \"**/*.tgz\", \"**/*.tar.gz\", \"**/*.tar\"]\n\
         \n\
         concurrency:\n\
         \x20 group: hauksbee-${{{{ github.workflow }}}}-${{{{ github.ref }}}}\n\
         \x20 cancel-in-progress: true\n\
         \n\
         jobs:\n\
         \x20 hauksbee:\n\
         \x20   runs-on: ubuntu-latest\n\
         \x20   timeout-minutes: 45\n\
         \x20   steps:\n\
         \x20     - uses: actions/checkout@11d5960a326750d5838078e36cf38b85af677262 # v4.4.0\n\
         \x20       with:\n\
         \x20         persist-credentials: false\n\
         \x20       # Pinned to the exact release commit: a tag can move after\n\
         \x20       # review, while an object ID cannot redirect the Action code.\n\
         \x20     - uses: hauksbee-dev/hauksbee/integrations/github-action@{}\n\
         \x20       with:\n\
         \x20         hauksbee-ref: {}\n\
         {}\
         \x20         junit: hauksbee-ci-results.xml\n\
         \x20         publish-report: ${{{{ github.event_name != 'pull_request' || github.event.pull_request.head.repo.full_name == github.repository }}}}\n",
        release_commit,
        release_commit,
        acquisition
    ))
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
            let has_own_logic = !is_bare_shebang(&remainder);
            if has_own_logic {
                park_local_hook(&local, &remainder)?;
            }
            write_executable(&hook, &script)?;
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
        write_executable(&hook, &script)?;
        return Ok(format!(
            "moved your existing hook to {} and installed {}, which runs the moved \
             hook FIRST and blocks the commit if it fails\n{}",
            local.display(),
            hook.display(),
            install_next_steps(&root)
        ));
    }
    write_executable(&hook, &plain_hook_script())?;
    Ok(format!(
        "installed {}\n{}",
        hook.display(),
        install_next_steps(&root)
    ))
}

/// Is this hook text nothing but (at most) a shebang: no logic worth keeping?
fn is_bare_shebang(text: &str) -> bool {
    let t = text.trim();
    t.is_empty() || t == "#!/bin/sh"
}

/// Rejoin the surviving lines of an edited file, with trailing blank lines
/// dropped and a final newline restored; empty when nothing survived.
fn join_kept(kept: &[&str]) -> String {
    let kept = kept
        .iter()
        .rposition(|l| !l.trim().is_empty())
        .map_or(&kept[..0], |last| &kept[..=last]);
    if kept.is_empty() {
        String::new()
    } else {
        kept.join("\n") + "\n"
    }
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
    write_executable(local, &parked)
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
    let kept: Vec<&str> = [&lines[..begin], &lines[end + 1..]].concat();
    Some(join_kept(&kept))
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
        write_executable(&hook, &parked)?;
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
    if is_bare_shebang(&remainder) {
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
    let indent_of = |l: &str| l.len() - l.trim_start().len();
    let indent = indent_of(lines[start]);
    // The entry ends at the next sibling list item or anything dedented to
    // (or past) the entry's own level; blank lines inside it are its own.
    let end = lines[start + 1..]
        .iter()
        .position(|line| !line.trim().is_empty() && indent_of(line) <= indent)
        .map_or(lines.len(), |i| start + 1 + i);
    let kept: Vec<&str> = [&lines[..start], &lines[end..]].concat();
    join_kept(&kept)
}

/// Write a hook script and mark it executable (on unix; elsewhere the write
/// alone is what git needs).
fn write_executable(path: &Path, text: &str) -> anyhow::Result<()> {
    fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(perms.mode() | 0o111);
        fs::set_permissions(path, perms)?;
    }
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
             the repo you want it in",
            cwd.display()
        );
    };
    let path = &if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let yaml = try_github_workflow_yaml()?;
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

    fn git_repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        assert!(Command::new("git")
            .args(["init", "-q"])
            .current_dir(tmp.path())
            .status()
            .expect("git init")
            .success());
        tmp
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }

    #[test]
    fn plain_hook_install_is_idempotent_and_writes_a_runnable_gate() {
        let tmp = git_repo();
        let first = hook_install(tmp.path()).unwrap();
        assert!(first.starts_with("installed"), "{first}");
        let hook = tmp.path().join(".git/hooks/pre-commit");
        let text = read(&hook);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_ne!(fs::metadata(&hook).unwrap().permissions().mode() & 0o111, 0);
        }
        // The block is delimited for uninstall, runs the specs, and records
        // the installing build so a stale hook can say so.
        assert!(text.contains(MARKER) && text.contains(END_MARKER), "{text}");
        assert!(text.contains("hauksbee-ci run"), "{text}");
        assert!(
            text.contains(&format!("installed_by='{}'", installed_by())),
            "{text}"
        );
        let status = Command::new("sh")
            .arg("-n")
            .arg(&hook)
            .status()
            .expect("sh -n");
        assert!(status.success(), "sh -n rejected the generated hook script");

        let second = hook_install(tmp.path()).unwrap();
        assert!(second.starts_with("already installed"), "{second}");
        assert_eq!(read(&hook), text);

        // A block written by a different build is refreshed in place.
        let stale = text.replace(
            &format!("installed_by='{}'", installed_by()),
            "installed_by='hauksbee-ci 0.0.0 (git dead)'",
        );
        fs::write(&hook, stale).unwrap();
        let msg = hook_install(tmp.path()).unwrap();
        assert!(msg.starts_with("refreshed"), "{msg}");
        assert_eq!(read(&hook), text);
    }

    #[test]
    fn an_existing_hook_is_moved_aside_chained_first_and_restored_on_uninstall() {
        let tmp = git_repo();
        let hook = tmp.path().join(".git/hooks/pre-commit");
        let local = tmp.path().join(".git/hooks/pre-commit.local");
        let original = "#!/bin/sh\necho preexisting\nexit 0\n";
        fs::write(&hook, original).unwrap();
        let msg = hook_install(tmp.path()).unwrap();
        assert!(msg.contains("pre-commit.local"), "{msg}");
        let text = read(&hook);
        assert!(
            text.contains(MARKER) && !text.contains("echo preexisting"),
            "{text}"
        );
        assert!(read(&local).contains("echo preexisting"));
        // The local hook runs BEFORE the gate: after its `exit 0` the gate
        // would never run.
        let chain = text
            .find("pre-commit.local")
            .expect("chains the local hook");
        assert!(
            chain < text.find("hauksbee-ci run").expect("runs specs"),
            "{text}"
        );
        // Idempotent: a second install does not chain the local hook twice.
        assert!(hook_install(tmp.path())
            .unwrap()
            .starts_with("already installed"));
        assert_eq!(read(&hook), text);

        let msg = hook_uninstall(tmp.path()).unwrap();
        assert!(msg.contains("restored"), "{msg}");
        assert_eq!(read(&hook), original);
        assert!(!local.exists());
    }

    #[test]
    fn install_repairs_a_legacy_appended_block_and_refuses_a_taken_local_slot() {
        // The user's hook with our block appended AFTER its `exit 0`: the
        // gate was unreachable. Re-installing parks the user's half in the
        // local hook and leaves a hook whose gate runs.
        let tmp = git_repo();
        let hook = tmp.path().join(".git/hooks/pre-commit");
        let body = plain_hook_script().replace("#!/bin/sh\n", "");
        fs::write(&hook, format!("#!/bin/sh\necho mine\nexit 0\n\n{body}")).unwrap();
        let msg = hook_install(tmp.path()).unwrap();
        assert!(msg.contains("never ran"), "{msg}");
        assert_eq!(read(&hook), plain_hook_script());
        assert!(read(&tmp.path().join(".git/hooks/pre-commit.local")).contains("echo mine"));

        // With the local slot already taken, nothing is touched.
        let tmp = git_repo();
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
        assert!(read(&tmp.path().join(".git/hooks/pre-commit")).contains("echo preexisting"));
    }

    #[test]
    fn uninstall_removes_only_what_install_wrote() {
        // Entirely ours: removed, and a second uninstall is not an error.
        let tmp = git_repo();
        hook_install(tmp.path()).unwrap();
        let hook = tmp.path().join(".git/hooks/pre-commit");
        assert!(hook_uninstall(tmp.path()).unwrap().starts_with("removed"));
        assert!(!hook.exists());
        assert!(hook_uninstall(tmp.path())
            .unwrap()
            .starts_with("nothing to uninstall"));

        // A hand-edited hook wrapping our block keeps the rest.
        let body = plain_hook_script().replace("#!/bin/sh\n", "");
        fs::write(&hook, format!("#!/bin/sh\n{body}\necho after\n")).unwrap();
        hook_uninstall(tmp.path()).unwrap();
        let text = read(&hook);
        assert!(
            text.contains("echo after") && !text.contains(MARKER),
            "{text}"
        );

        // A hook we did not write is refused untouched.
        fs::write(&hook, "#!/bin/sh\necho someone else\n").unwrap();
        let err = hook_uninstall(tmp.path()).unwrap_err().to_string();
        assert!(err.contains("refusing"), "{err}");
        assert!(read(&hook).contains("someone else"));
    }

    #[test]
    fn pre_commit_config_entry_is_appended_after_existing_hooks_and_removed_cleanly() {
        let tmp = git_repo();
        let config = tmp.path().join(".pre-commit-config.yaml");
        fs::write(
            &config,
            "repos:\n  - repo: https://github.com/psf/black\n    rev: 24.1.0\n    hooks:\n      - id: black\n",
        )
        .unwrap();
        let msg = hook_install(tmp.path()).unwrap();
        assert!(msg.contains("pre-commit install"), "{msg}");
        let text = read(&config);
        let line_of = |needle: &str| text.lines().position(|l| l.contains(needle)).unwrap();
        // Appended, not prepended: the fast formatters keep running first.
        assert!(line_of("repos:") < line_of("psf/black"), "{text}");
        assert!(
            line_of("psf/black") < line_of("hauksbee-dev/hauksbee"),
            "{text}"
        );
        assert!(text.contains("id: hauksbee-ci") && text.contains("id: black"));
        assert!(hook_install(tmp.path())
            .unwrap()
            .starts_with("already installed"));

        let msg = hook_uninstall(tmp.path()).unwrap();
        assert!(msg.contains("removed the hauksbee-ci entry"), "{msg}");
        let text = read(&config);
        assert!(
            !text.contains("hauksbee") && text.contains("id: black") && text.contains("repos:"),
            "{text}"
        );
    }

    #[test]
    fn workflow_write_is_idempotent_refuses_divergence_and_lands_at_the_repo_root() {
        let tmp = git_repo();
        let path = tmp.path().join(".github/workflows/hauksbee.yml");
        assert!(github_action_write(tmp.path(), &path)
            .unwrap()
            .starts_with("wrote"));
        assert!(github_action_write(tmp.path(), &path)
            .unwrap()
            .starts_with("already up to date"));
        fs::write(&path, "something else\n").unwrap();
        let err = github_action_write(tmp.path(), &path)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not overwriting"), "{err}");

        // GitHub only reads .github/workflows at the top of the repo, so a
        // relative --write from a subdirectory lands at the root.
        let tmp = git_repo();
        let sub = tmp.path().join("hardware/ci");
        fs::create_dir_all(&sub).unwrap();
        github_action_write(&sub, Path::new(".github/workflows/hauksbee.yml")).unwrap();
        assert!(tmp.path().join(".github/workflows/hauksbee.yml").exists());
        assert!(!sub.join(".github").exists());

        // Outside a git repo it refuses, like `hook install` does.
        let tmp = tempfile::tempdir().unwrap();
        let err = github_action_write(tmp.path(), Path::new(".github/workflows/hauksbee.yml"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not inside a git repository"), "{err}");
        assert!(!tmp.path().join(".github").exists());
    }

    #[test]
    fn generated_workflow_pins_the_public_action_and_is_valid_yaml() {
        let source = Some("0123456789abcdef0123456789abcdef01234567");
        let release_tag = format!("v{}", env!("CARGO_PKG_VERSION"));
        for (mode, tag) in [("source", None), ("release", Some(release_tag.as_str()))] {
            let yaml = github_workflow_yaml_for(source, tag).unwrap();
            assert!(yaml.contains("persist-credentials: false"), "{yaml}");
            let parsed = yaml_rust2::YamlLoader::load_from_str(&yaml).unwrap_or_else(|error| {
                panic!("{mode} workflow is not valid YAML: {error}\n{yaml}")
            });
            assert!(
                matches!(parsed.as_slice(), [yaml_rust2::Yaml::Hash(_)]),
                "{mode} workflow should be exactly one YAML mapping: {yaml}"
            );
            // The public Action is used directly and pinned to an exact
            // commit; no credential or secret reference is emitted.
            let uses_line = yaml
                .lines()
                .find(|line| {
                    line.trim_start()
                        .starts_with("- uses: hauksbee-dev/hauksbee/integrations/github-action@")
                })
                .expect("pinned public action reference");
            let pinned_ref = uses_line.rsplit('@').next().unwrap();
            assert!(
                pinned_ref.len() == 40 && pinned_ref.bytes().all(|b| b.is_ascii_hexdigit()),
                "{yaml}"
            );
            assert!(
                !yaml.contains("secrets.") && !yaml.contains("hauksbee-token:"),
                "{yaml}"
            );
            for path in ["**/*.xml", "**/*.zip", "**/*.tar.gz"] {
                assert!(
                    yaml.contains(path),
                    "{mode} workflow omitted {path}:\n{yaml}"
                );
            }
            // A source build compiles from the pinned ref; an exact release
            // build uses the prebuilt binary.
            if tag.is_some() {
                assert!(
                    yaml.contains(&format!("hauksbee-version: {release_tag}")),
                    "{yaml}"
                );
                assert!(yaml.contains("prefer-prebuilt: true"), "{yaml}");
            } else {
                assert!(
                    yaml.contains("hauksbee-ref:") && !yaml.contains("hauksbee-version:"),
                    "{yaml}"
                );
                assert!(yaml.contains("prefer-prebuilt: false"), "{yaml}");
            }
        }
        for source in [None, Some("0000")] {
            let err = github_workflow_yaml_for(source, None)
                .unwrap_err()
                .to_string();
            assert!(err.contains("no verified Hauksbee source commit"), "{err}");
        }
    }

    #[test]
    fn green_next_step_names_only_the_missing_wiring() {
        let tmp = git_repo();
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
