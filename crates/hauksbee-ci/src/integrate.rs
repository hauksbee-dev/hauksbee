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
        let is_workflow = path
            .extension()
            .is_some_and(|e| e == "yml" || e == "yaml");
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
/// The `# installed by` line records the exact build that wrote the hook, and
/// the script compares it against the live `hauksbee-ci --version` on every
/// run: a stale hook warns (one line, never blocks) with the refresh command.
/// Specs run one at a time so the script can count RED ones honestly; the
/// blocked-commit line reports that count and the `--no-verify` escape hatch.
fn plain_hook_script() -> String {
    let installed = installed_by();
    format!(
        "#!/bin/sh\n\
         # {MARKER}: block the commit when a staged change breaks a hauksbee-ci spec.\n\
         # installed by {installed}\n\
         # Refresh with `hauksbee-ci hook install`; remove with `hauksbee-ci hook uninstall`.\n\
         if ! command -v hauksbee-ci >/dev/null 2>&1; then\n\
         \x20 echo 'hauksbee-ci: binary not on PATH; skipping hardware check' >&2\n\
         \x20 exit 0\n\
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
         \x20   # A hauksbee-ci spec is a TOML file with a top-level `board = ...`.\n\
         \x20   specs=$(grep -l '^board *=' ci/*.toml *.toml 2>/dev/null || true)\n\
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
         \x20     - uses: hauksbee-dev/hauksbee/integrations/github-action@v{}\n\
         \x20       with:\n\
         \x20         junit: hauksbee-ci-results.xml\n",
        env!("CARGO_PKG_VERSION")
    )
}

/// Count the specs the installed hook will discover, mirroring its grep
/// exactly: `*.toml` files in `ci/` and the repo root whose text has a
/// top-level `board =` line. The install output reports this number so a
/// user learns "the hook found nothing to run" at install time, not at their
/// next commit.
fn count_discoverable_specs(root: &Path) -> usize {
    let mut n = 0;
    for dir in [root.join("ci"), root.to_path_buf()] {
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
    let discovered = if n == 0 {
        "discovered 0 specs in ci/ and the repo root; the hook is a no-op until \
         one exists (`hauksbee-ci init <board>` scaffolds one)"
            .to_string()
    } else {
        format!("discovered {n} spec(s) in ci/ and the repo root")
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
        let text = fs::read_to_string(&config)
            .with_context(|| format!("reading {}", config.display()))?;
        if text.contains("hauksbee") {
            return Ok(format!(
                "already installed: {} already references hauksbee; nothing changed",
                config.display()
            ));
        }
        // Insert the entry directly under the top-level `repos:` key, so it
        // stays inside the list no matter what follows the list in the file.
        // That PREPENDS it before any existing entries, and the outcome line
        // says so rather than leaving the user to diff the file.
        let entry = pre_commit_entry();
        let (new_text, did) = if let Some(pos) = text
            .lines()
            .position(|l| l.trim_end() == "repos:")
        {
            let mut lines: Vec<&str> = text.lines().collect();
            lines.insert(pos + 1, entry.trim_end());
            let mut joined = lines.join("\n");
            joined.push('\n');
            (
                joined,
                "prepended the hauksbee-ci entry under `repos:` (before your existing hooks)",
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
        fs::write(&config, new_text)
            .with_context(|| format!("writing {}", config.display()))?;
        return Ok(format!(
            "{did} in {}; run `pre-commit install` to activate it\n{}",
            config.display(),
            install_next_steps(&root)
        ));
    }

    // No pre-commit framework: plain git hook.
    let hooks_dir = root.join(".git/hooks");
    fs::create_dir_all(&hooks_dir)
        .with_context(|| format!("creating {}", hooks_dir.display()))?;
    let hook = hooks_dir.join("pre-commit");
    if hook.exists() {
        let text =
            fs::read_to_string(&hook).with_context(|| format!("reading {}", hook.display()))?;
        if text.contains(MARKER) {
            // Same build wrote it: nothing to do. A different build's block
            // gets refreshed in place, because the hook's own stale-build
            // warning tells the user `hook install` is the fix.
            if text.contains(&format!("installed_by='{}'", installed_by())) {
                return Ok(format!(
                    "already installed: {} carries the hauksbee-ci block; nothing changed",
                    hook.display()
                ));
            }
            let Some(remainder) = strip_hook_block(&text) else {
                bail!(
                    "{} carries a hauksbee-ci block this build cannot safely \
                     replace (no `{END_MARKER}` line); edit the file by hand",
                    hook.display()
                );
            };
            let new_text = append_hook_block(&remainder);
            fs::write(&hook, new_text)
                .with_context(|| format!("writing {}", hook.display()))?;
            set_executable(&hook)?;
            return Ok(format!(
                "refreshed the hauksbee-ci block in {} (a different hauksbee-ci build wrote it)\n{}",
                hook.display(),
                install_next_steps(&root)
            ));
        }
        // Preserve the existing hook: append our block (minus the shebang).
        let new_text = append_hook_block(&text);
        fs::write(&hook, new_text)
            .with_context(|| format!("writing {}", hook.display()))?;
        set_executable(&hook)?;
        return Ok(format!(
            "appended the hauksbee-ci block to your existing {}\n{}",
            hook.display(),
            install_next_steps(&root)
        ));
    }
    fs::write(&hook, plain_hook_script())
        .with_context(|| format!("writing {}", hook.display()))?;
    set_executable(&hook)?;
    Ok(format!(
        "installed {}\n{}",
        hook.display(),
        install_next_steps(&root)
    ))
}

/// Append the hauksbee-ci block (minus the shebang) to existing hook text; an
/// empty remainder gets the full standalone script back.
fn append_hook_block(existing: &str) -> String {
    if existing.trim().is_empty() || existing.trim() == "#!/bin/sh" {
        return plain_hook_script();
    }
    let block = plain_hook_script();
    let block_body = block.strip_prefix("#!/bin/sh\n").unwrap_or(&block);
    let mut new_text = existing.to_string();
    if !new_text.ends_with('\n') {
        new_text.push('\n');
    }
    new_text.push('\n');
    // The appended block must not `exit 0` past any hook logic that
    // follows it in future edits; as the last block that is fine, and the
    // early exits inside only fire on failure.
    new_text.push_str(block_body);
    new_text
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

    let hook = root.join(".git/hooks/pre-commit");
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
        if line_indent < indent
            || (line_indent == indent && !line.trim_start().starts_with("- "))
        {
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

/// `hauksbee-ci github-action --write <path>`: write the workflow file.
/// Idempotent: an identical existing file is a no-op; a different one is
/// refused rather than clobbered.
pub fn github_action_write(path: &Path) -> anyhow::Result<String> {
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
            fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
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
        (false, true) => Some(
            "next: gate commits locally too: `hauksbee-ci hook install`".to_string(),
        ),
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
    fn plain_hook_appends_to_existing_hook() {
        let tmp = tempfile::tempdir().unwrap();
        git_repo(tmp.path());
        let hook = tmp.path().join(".git/hooks/pre-commit");
        fs::write(&hook, "#!/bin/sh\necho preexisting\n").unwrap();
        let msg = hook_install(tmp.path()).unwrap();
        assert!(msg.starts_with("appended"), "{msg}");
        let text = fs::read_to_string(&hook).unwrap();
        assert!(text.contains("echo preexisting"));
        assert!(text.contains(MARKER));
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
        let hauksbee_line = text
            .lines()
            .position(|l| l.contains("hauksbee-dev/hauksbee"))
            .unwrap();
        assert_eq!(hauksbee_line, repos_line + 1, "entry sits under repos:\n{text}");
        assert!(text.contains("id: hauksbee-ci"));
        assert!(text.contains("id: black"));
        // Idempotent.
        let again = hook_install(tmp.path()).unwrap();
        assert!(again.starts_with("already installed"), "{again}");
    }

    #[test]
    fn workflow_write_is_idempotent_and_refuses_divergence() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".github/workflows/hauksbee.yml");
        let first = github_action_write(&path).unwrap();
        assert!(first.starts_with("wrote"), "{first}");
        let second = github_action_write(&path).unwrap();
        assert!(second.starts_with("already up to date"), "{second}");
        fs::write(&path, "something else\n").unwrap();
        let err = github_action_write(&path).unwrap_err().to_string();
        assert!(err.contains("not overwriting"), "{err}");
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
        assert!(text.contains(&format!("# installed by {installed}")), "{text}");
        assert!(text.contains(&format!("installed_by='{installed}'")), "{text}");
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
        let stale = fs::read_to_string(&hook)
            .unwrap()
            .replace(&format!("installed_by='{}'", installed_by()), "installed_by='hauksbee-ci 0.0.0 (git dead)'");
        fs::write(&hook, stale).unwrap();
        let msg = hook_install(tmp.path()).unwrap();
        assert!(msg.starts_with("refreshed"), "{msg}");
        let text = fs::read_to_string(&hook).unwrap();
        assert!(text.contains(&format!("installed_by='{}'", installed_by())), "{text}");
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
        let tmp = tempfile::tempdir().unwrap();
        git_repo(tmp.path());
        let hook = tmp.path().join(".git/hooks/pre-commit");
        fs::write(&hook, "#!/bin/sh\necho preexisting\n").unwrap();
        hook_install(tmp.path()).unwrap();
        let msg = hook_uninstall(tmp.path()).unwrap();
        assert!(msg.contains("rest of your hook is untouched"), "{msg}");
        let text = fs::read_to_string(&hook).unwrap();
        assert!(text.contains("echo preexisting"), "{text}");
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
        assert!(installed.contains("prepended"), "{installed}");
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
        let path = tmp.path().join(".github/workflows/hauksbee.yml");
        let msg = github_action_write(&path).unwrap();
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
        github_action_write(&tmp.path().join(".github/workflows/hauksbee.yml")).unwrap();
        assert_eq!(green_next_step(tmp.path()), None);
    }
}
