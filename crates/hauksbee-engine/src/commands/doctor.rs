//! The `hauksbee doctor [--backends] [--json]` subcommand: probe co-sim backend
//! availability (AVR, QEMU, Renode) and print one machine-readable line per
//! backend. It calls the engine's OWN backend resolvers rather than
//! re-implementing discovery, so what `doctor` reports can never drift from what a
//! real co-sim would actually accept.

/// `hauksbee doctor --backends`: report co-sim backend availability using the
/// engine's OWN discovery, so `scripts/doctor.sh` can never drift from what a
/// real co-sim would accept.
///
/// For each backend this calls the exact resolver the scheduler uses
/// (`hauksbee_mcu::qemu::find_qemu`, `hauksbee_mcu::renode::find_renode`) — no
/// re-implemented search logic. `find_qemu` runs the Espressif-fork check
/// (`is_esp_fork`), so a Homebrew mainline `qemu-system-xtensa` on PATH is
/// reported `absent` here just as the co-sim rejects it, and a fork under
/// `~/.hauksbee-qemu-esp` or a Renode under `~/renode-portable` is reported
/// present with its resolved path.
///
/// stdout: one line per backend, `NAME<TAB>STATUS<TAB>DETAIL`, STATUS a single
/// lowercase token (`ok` / `absent` / `builtin` / `disabled`); DETAIL is the
/// resolved path or a one-line install hint and may contain spaces (parsers
/// should read field 3 to end-of-line). The human header goes to stderr so the
/// data stream stays clean.
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
        detail: "simavr linked into this binary".to_string(),
        summary: "ATmega / ATtiny firmware co-sim",
    });
    #[cfg(not(feature = "avr"))]
    backends.push(Backend {
        name: "avr",
        status: "disabled",
        detail: "compiled out — rebuild with the default features + libsimavr \
                 (scripts/install-sims.sh --avr)"
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

    // Human framing on stderr; the data table on stdout stays parseable.
    eprintln!("hauksbee co-sim backends (resolved by the engine's own discovery)");
    for b in &backends {
        eprintln!("    {:<13} {}", b.name, b.summary);
        println!("{}\t{}\t{}", b.name, b.status, b.detail);
    }
    Ok(())
}

/// Collapse a possibly multi-line message to its first line (discovery errors
/// are one line today, but this keeps the doctor table one-row-per-backend even
/// if a resolver's message grows).
fn one_line(msg: &str) -> String {
    msg.lines().next().unwrap_or("").to_string()
}
