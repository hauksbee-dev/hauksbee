//! `hauksbee install esp-qemu [--yes]`: fetch Espressif's official prebuilt
//! QEMU fork (qemu-system-xtensa + qemu-system-riscv32) into
//! `~/.hauksbee-qemu-esp/`, the first location the engine's own discovery
//! checks. Fetch, never bundle: the fork is GPL-2.0 and stays out of this
//! MIT-licensed tree; the installer downloads from
//! `github.com/espressif/qemu` releases, verifies the sha256 against the
//! release's checksum manifest, and accepts each binary only after the same
//! `is_esp_fork` machine-list check a co-sim applies.
//!
//! The interactive half lives here too: [`offer_esp_qemu_install`] is the
//! co-sim pre-flight `hauksbee run` calls when a board needs a `qemu:` core
//! whose emulator is absent, on a TTY it offers to install inline and the
//! run then proceeds; declined / non-TTY paths keep the loud install-guidance
//! error exactly as before. The prompt lives at the CLI layer on purpose: the
//! engine library and server must never block on stdin.

/// Run the `install esp-qemu` subcommand. `yes` skips the confirmation
/// prompt (CI); without it, a non-interactive stdin refuses rather than
/// downloading on a guess.
#[cfg(feature = "qemu")]
pub fn esp_qemu(yes: bool) -> anyhow::Result<()> {
    use hauksbee_mcu::qemu::{install, QemuArch};

    let arches = [QemuArch::Xtensa, QemuArch::Riscv32];
    let missing: Vec<QemuArch> = arches
        .iter()
        .copied()
        .filter(|&a| !hauksbee_mcu::qemu::is_available(a))
        .collect();
    if missing.is_empty() {
        for a in arches {
            let p = hauksbee_mcu::qemu::find_qemu(a)?;
            println!("{}\talready installed\t{}", a.binary_name(), p.display());
        }
        return Ok(());
    }

    let root = install::install_root()?;
    eprintln!(
        "This downloads Espressif's official prebuilt QEMU fork (GPL-2.0, \
         built and published by Espressif at https://github.com/espressif/qemu/releases)\n\
         and unpacks it into {} — it is a separate program hauksbee talks to \
         over sockets, not a part of hauksbee.",
        root.display()
    );
    if !yes && !confirm("Proceed with download and install? [y/N] ")? {
        anyhow::bail!(
            "install declined. Re-run `hauksbee install esp-qemu` (add --yes to \
             skip this prompt), or install manually: docs/cosim/SIMULATORS.md"
        );
    }

    let mut progress = |msg: &str| eprintln!("  {msg}");
    let bins = install::install_esp_qemu(&missing, &mut progress)?;
    for b in &bins {
        println!("installed\t{}", b.display());
    }
    eprintln!("Done. `hauksbee doctor --backends` will now report these as ok.");
    Ok(())
}

#[cfg(not(feature = "qemu"))]
pub fn esp_qemu(_yes: bool) -> anyhow::Result<()> {
    anyhow::bail!(
        "this build of hauksbee was compiled without the `qemu` feature, so it \
         could not use an Espressif QEMU even if installed; rebuild with \
         --features qemu first"
    )
}

/// Co-sim pre-flight for `hauksbee run --firmware` (and anything else CLI-side
/// that is about to boot a `qemu:` core): when one of `backends` is a
/// `qemu:<part>` whose emulator binary is absent, offer, on a real TTY, to
/// install it inline, so the run can continue instead of dying with the
/// install-guidance error. Returns `Ok(())` both when nothing was needed and
/// after a successful install; declining is also `Ok(())` (the co-sim then
/// fails downstream with the existing loud error, which stays the single
/// source of truth for the non-interactive path).
#[cfg(feature = "qemu")]
pub fn offer_esp_qemu_install(backends: &[String]) -> anyhow::Result<()> {
    use hauksbee_mcu::qemu::{install, QemuArch};
    use hauksbee_mcu::SocConfig;
    use std::io::IsTerminal;

    // Which arches do the bound qemu: backends need, and which are absent?
    // The arch comes from the SoC descriptor (data), not a name heuristic.
    let mut missing: Vec<QemuArch> = Vec::new();
    for b in backends {
        if !b.starts_with("qemu:") {
            continue;
        }
        let arch = match SocConfig::resolve(b) {
            Ok(SocConfig::Qemu(cfg)) => cfg.arch,
            // Unknown part / renode descriptor: the scheduler's own resolve
            // will produce the real error; nothing to offer here.
            _ => continue,
        };
        if !hauksbee_mcu::qemu::is_available(arch) && !missing.contains(&arch) {
            missing.push(arch);
        }
    }
    if missing.is_empty() {
        return Ok(());
    }

    // Non-interactive (CI, pipes): keep the existing loud error downstream.
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Ok(());
    }

    let names: Vec<&str> = missing.iter().map(|a| a.binary_name()).collect();
    eprintln!(
        "This board's MCU co-simulates through Espressif QEMU, but {} {} not \
         installed.",
        names.join(" and "),
        if names.len() == 1 { "is" } else { "are" },
    );
    eprintln!(
        "hauksbee can download Espressif's official prebuilt fork (GPL-2.0) \
         from https://github.com/espressif/qemu/releases into {} now.",
        install::install_root()?.display()
    );
    if !confirm("Download and install it, then continue? [y/N] ")? {
        eprintln!("Skipping install; the run will fail with install guidance.");
        return Ok(());
    }
    let mut progress = |msg: &str| eprintln!("  {msg}");
    install::install_esp_qemu(&missing, &mut progress)?;
    eprintln!("Espressif QEMU installed; continuing the run.");
    Ok(())
}

#[cfg(not(feature = "qemu"))]
pub fn offer_esp_qemu_install(_backends: &[String]) -> anyhow::Result<()> {
    Ok(())
}

/// One-line y/N prompt on stderr, reading a line from stdin. Refuses (returns
/// an error) when stdin is not a terminal, a piped stdin must never be able
/// to "answer" a consent prompt.
#[cfg(feature = "qemu")]
fn confirm(prompt: &str) -> anyhow::Result<bool> {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "stdin is not a terminal, refusing to prompt; pass --yes for \
             non-interactive installs"
        );
    }
    eprint!("{prompt}");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let ans = line.trim().to_ascii_lowercase();
    Ok(ans == "y" || ans == "yes")
}
