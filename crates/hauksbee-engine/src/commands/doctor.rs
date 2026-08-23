//! The `hauksbee doctor [--backends] [--json]` subcommand: probe co-sim backend
//! availability (AVR, QEMU, Renode) and print one machine-readable line per
//! backend. It calls the engine's OWN backend resolvers rather than
//! re-implementing discovery, so what `doctor` reports can never drift from what a
//! real co-sim would actually accept.

/// `hauksbee doctor --backends`: report co-sim backend availability using the
/// engine's OWN discovery, so no other availability surface can drift from
/// what a real co-sim would accept.
///
/// For each backend this calls the exact resolver the scheduler uses
/// (`hauksbee_mcu::qemu::find_qemu`, `hauksbee_mcu::renode::find_renode`), no
/// re-implemented search logic. `find_qemu` runs the Espressif-fork check
/// (`is_esp_fork`), so a Homebrew mainline `qemu-system-xtensa` on PATH is
/// reported `absent` here just as the co-sim rejects it, and a fork under
/// `~/.hauksbee-qemu-esp` or a Renode under `~/renode-portable` is reported
/// present with its resolved path.
///
/// Piped stdout: one line per backend, `NAME<TAB>STATUS<TAB>DETAIL`, STATUS a
/// single lowercase token (`ok` / `absent` / `builtin` / `disabled`); DETAIL
/// is the resolved path or a one-line install hint and may contain spaces
/// (parsers should read field 3 to end-of-line). The human header goes to
/// stderr so the data stream stays clean. On a TTY there is no parser to
/// protect, so the view is one box-drawing table instead of TSV interleaved
/// with stderr framing.
pub fn run(_backends: bool, json: bool) -> anyhow::Result<()> {
    // A probed backend. `status` is a single token by contract (see above).
    struct Backend {
        name: &'static str,
        status: &'static str,
        detail: String,
        summary: &'static str,
    }

    let mut backends: Vec<Backend> = Vec::new();

    // AVR: built into this binary via libsimavr (feature `avr`); there is no
    // external process to locate, so it is `builtin` when compiled in.
    #[cfg(feature = "avr")]
    backends.push(Backend {
        name: "avr",
        status: "builtin",
        detail: hauksbee_mcu::simavr_build_detail(),
        summary: "ATmega / ATtiny firmware co-sim",
    });
    #[cfg(not(feature = "avr"))]
    backends.push(Backend {
        name: "avr",
        status: "disabled",
        // Honest for BINARY users too: this build (the permissive download)
        // can never gain AVR at runtime; rebuilding from source with
        // libsimavr is the only way to get it.
        detail: "not in this build (the permissive, Apache-2.0 download drops \
                 the GPL simavr backend). For AVR co-sim, build from source \
                 with libsimavr (scripts/install-sims.sh --avr)"
            .to_string(),
        summary: "ATmega / ATtiny firmware co-sim",
    });

    // Espressif QEMU (Xtensa ESP32 / ESP32-S3, RISC-V ESP32-C3). `find_qemu`
    // verifies the binary is the Espressif fork before accepting it.
    #[cfg(feature = "qemu")]
    {
        use hauksbee_mcu::qemu::{find_qemu, QemuArch};
        let probes = [
            (
                "qemu-xtensa",
                QemuArch::Xtensa,
                "ESP32 / ESP32-S3 firmware co-sim (Espressif QEMU fork)",
            ),
            (
                "qemu-riscv32",
                QemuArch::Riscv32,
                "ESP32-C3 firmware co-sim (Espressif QEMU fork)",
            ),
        ];
        for (name, arch, summary) in probes {
            match find_qemu(arch) {
                Ok(p) => backends.push(Backend {
                    name,
                    status: "ok",
                    detail: p.display().to_string(),
                    summary,
                }),
                Err(e) => backends.push(Backend {
                    name,
                    status: "absent",
                    detail: one_line(&e.to_string()),
                    summary,
                }),
            }
        }
    }
    #[cfg(not(feature = "qemu"))]
    for (name, summary) in [
        ("qemu-xtensa", "ESP32 / ESP32-S3 firmware co-sim"),
        ("qemu-riscv32", "ESP32-C3 firmware co-sim"),
    ] {
        backends.push(Backend {
            name,
            status: "disabled",
            detail: "built without the `qemu` feature".to_string(),
            summary,
        });
    }

    // Renode (STM32 / nRF52 / SiFive RISC-V, i.e. ARM Cortex-M and RISC-V).
    #[cfg(feature = "renode")]
    match hauksbee_mcu::renode::find_renode() {
        Ok(p) => backends.push(Backend {
            name: "renode",
            status: "ok",
            detail: p.display().to_string(),
            summary: "STM32 / nRF52 / RISC-V firmware co-sim",
        }),
        Err(e) => backends.push(Backend {
            name: "renode",
            status: "absent",
            detail: one_line(&e.to_string()),
            summary: "STM32 / nRF52 / RISC-V firmware co-sim",
        }),
    }
    #[cfg(not(feature = "renode"))]
    backends.push(Backend {
        name: "renode",
        status: "disabled",
        detail: "built without the `renode` feature".to_string(),
        summary: "STM32 / nRF52 / RISC-V firmware co-sim",
    });

    if json {
        let arr: Vec<serde_json::Value> = backends
            .iter()
            .map(|b| {
                serde_json::json!({
                    "name": b.name,
                    "status": b.status,
                    "available": b.status == "ok" || b.status == "builtin",
                    "detail": b.detail,
                    "summary": b.summary,
                })
            })
            .collect();
        println!("{}", serde_json::json!({ "backends": arr }));
        return Ok(());
    }

    // A TTY gets ONE table a human can actually read, closed by one summary
    // line; nothing else (U10: an earlier shape interleaved two formats). A
    // pipe gets the TSV contract external tooling parses; its human framing
    // goes to stderr as one contiguous block BEFORE the data lines, never
    // alternating with them, so a `2>&1` merge stays two clean blocks.
    let available = backends
        .iter()
        .filter(|b| b.status == "ok" || b.status == "builtin")
        .count();
    let summary_line = format!(
        "{available} of {} backends available; `hauksbee install --help` fetches the missing ones.",
        backends.len()
    );
    if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        let rows: Vec<Vec<String>> = backends
            .iter()
            .map(|b| {
                vec![
                    b.name.to_string(),
                    b.status.to_string(),
                    b.summary.to_string(),
                    b.detail.clone(),
                ]
            })
            .collect();
        print!(
            "{}",
            super::models::box_table(&["Backend", "Status", "Co-sim", "Detail"], &rows)
        );
        println!("{summary_line}");
    } else {
        eprintln!("hauksbee co-sim backends (resolved by the engine's own discovery)");
        for b in &backends {
            eprintln!("    {:<13} {}", b.name, b.summary);
        }
        for b in &backends {
            println!("{}\t{}\t{}", b.name, b.status, b.detail);
        }
    }
    Ok(())
}

/// Collapse a possibly multi-line message to its first line (discovery errors
/// are one line today, but this keeps the doctor table one-row-per-backend even
/// if a resolver's message grows).
#[cfg(any(feature = "qemu", feature = "renode"))]
fn one_line(msg: &str) -> String {
    msg.lines().next().unwrap_or("").to_string()
}
