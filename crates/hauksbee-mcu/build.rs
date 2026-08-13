use std::env;
use std::fmt::Write as _;
use std::fs::File;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

fn sha256(path: &Path) -> String {
    let mut file =
        File::open(path).unwrap_or_else(|e| panic!("could not hash {}: {e}", path.display()));
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .unwrap_or_else(|e| panic!("could not hash {}: {e}", path.display()));
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    let mut output = String::with_capacity(64);
    for byte in hash.finalize() {
        write!(&mut output, "{byte:02x}").unwrap();
    }
    output
}

fn installed_simavr_headers(root: &Path) -> Vec<PathBuf> {
    fn visit(dir: &Path, out: &mut Vec<PathBuf>) {
        let entries = std::fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("could not enumerate {}: {e}", dir.display()));
        for entry in entries {
            let path = entry
                .unwrap_or_else(|e| panic!("could not enumerate {}: {e}", dir.display()))
                .path();
            if path.is_dir() {
                visit(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "h") {
                out.push(path);
            }
        }
    }

    let mut headers = Vec::new();
    visit(root, &mut headers);
    headers.sort();
    headers
}

fn main() {
    // Explicit rerun directives below disable Cargo's default package-wide
    // tracking. bindgen reads this local umbrella header, so it must remain an
    // explicit input alongside the installed simavr header tree.
    println!("cargo:rerun-if-changed=wrapper.h");
    // The simavr link + bindgen step is only needed for the `avr` backend.
    // When that feature is off (e.g. a renode-only build on a machine without
    // libsimavr), skip it entirely so the crate still compiles.
    if env::var_os("CARGO_FEATURE_AVR").is_none() {
        return;
    }

    // simavr's lib + headers live in different prefixes per platform/install.
    // Allow an explicit override (SIMAVR_LIB_DIR / SIMAVR_INCLUDE_DIR); otherwise
    // pick the conventional prefix for the target: Homebrew on Apple Silicon
    // (/opt/homebrew), /usr/local everywhere else (Linux, incl. CI where the
    // release workflow builds simavr into /usr/local, and Intel-Mac Homebrew).
    // Hardcoding /opt/homebrew previously made the Linux build link only by luck
    // via the LIBRARY_PATH env and broke Intel-Mac / from-source builds outright.
    let default_prefix = if cfg!(target_os = "macos") && cfg!(target_arch = "aarch64") {
        "/opt/homebrew"
    } else {
        "/usr/local"
    };
    let lib_dir = env::var("SIMAVR_LIB_DIR").unwrap_or_else(|_| format!("{default_prefix}/lib"));
    let include_dir =
        env::var("SIMAVR_INCLUDE_DIR").unwrap_or_else(|_| format!("{default_prefix}/include"));
    println!("cargo:rerun-if-env-changed=SIMAVR_LIB_DIR");
    println!("cargo:rerun-if-env-changed=SIMAVR_INCLUDE_DIR");
    println!("cargo:rerun-if-env-changed=SIMAVR_COMMIT");

    // CI and release builds place an immutable source marker beside the exact
    // headers/archive. Make that identity a Cargo input and embed it into the
    // crate, so a path-stable target cache cannot silently retain an object
    // linked against different GPL corresponding source.
    if let Ok(expected_commit) = env::var("SIMAVR_COMMIT") {
        if expected_commit.len() != 40 || !expected_commit.bytes().all(|b| b.is_ascii_hexdigit()) {
            panic!("SIMAVR_COMMIT must be one 40-character hexadecimal Git commit");
        }
        let include_root = std::fs::canonicalize(&include_dir)
            .unwrap_or_else(|e| panic!("could not resolve SIMAVR_INCLUDE_DIR {include_dir}: {e}"));
        let lib_root = std::fs::canonicalize(&lib_dir)
            .unwrap_or_else(|e| panic!("could not resolve SIMAVR_LIB_DIR {lib_dir}: {e}"));
        let prefix = include_root.parent().unwrap_or(Path::new("/"));
        let lib_prefix = lib_root.parent().unwrap_or(Path::new("/"));
        if prefix != lib_prefix {
            panic!(
                "SIMAVR_INCLUDE_DIR and SIMAVR_LIB_DIR must share one prefix when SIMAVR_COMMIT attests the linked source (got {} and {})",
                include_root.display(),
                lib_root.display()
            );
        }
        let marker = prefix.join(".hauksbee-simavr-commit");
        println!("cargo:rerun-if-changed={}", marker.display());
        let installed_commit = std::fs::read_to_string(&marker)
            .unwrap_or_else(|e| panic!("could not read {}: {e}", marker.display()));
        if installed_commit.trim() != expected_commit {
            panic!(
                "simavr provenance mismatch: build requested {expected_commit}, but {} records {}",
                marker.display(),
                installed_commit.trim()
            );
        }
        let payload_record = prefix.join(".hauksbee-simavr-payload.sha256");
        println!("cargo:rerun-if-changed={}", payload_record.display());
        let archive = lib_root.join("libsimavr.a");
        println!("cargo:rerun-if-changed={}", archive.display());
        let mut payload_lines = Vec::new();
        for header in installed_simavr_headers(&include_root.join("simavr")) {
            println!("cargo:rerun-if-changed={}", header.display());
            let relative = header.strip_prefix(prefix).unwrap_or_else(|_| {
                panic!(
                    "simavr header escaped {}: {}",
                    prefix.display(),
                    header.display()
                )
            });
            payload_lines.push(format!("{}  {}", sha256(&header), relative.display()));
        }
        payload_lines.push(format!("{}  lib/libsimavr.a", sha256(&archive)));
        let expected_payload = payload_lines.join("\n");
        let recorded_payload = std::fs::read_to_string(&payload_record)
            .unwrap_or_else(|e| panic!("could not read {}: {e}", payload_record.display()));
        if recorded_payload.trim() != expected_payload {
            panic!(
                "simavr payload digest mismatch under {}: installed headers/archive are not the bytes recorded after the pinned build",
                prefix.display()
            );
        }
        println!("cargo:rustc-env=HAUKSBEE_SIMAVR_COMMIT={expected_commit}");
    }

    // Preflight: the `avr` feature links a SYSTEM libsimavr (static) and runs
    // bindgen over its headers. If simavr is not installed, the raw failure is a
    // cryptic bindgen "header not found" or a linker error much further down.
    // Check the resolved layout up front and, if it is missing, fail with ONE
    // actionable message. `make install DESTDIR=<prefix>` puts the header at
    // <prefix>/include/simavr/sim_avr.h and the archive at <prefix>/lib/libsimavr.a.
    let header = format!("{include_dir}/simavr/sim_avr.h");
    let static_lib = format!("{lib_dir}/libsimavr.a");
    let header_present = Path::new(&header).exists();
    let lib_present = Path::new(&static_lib).exists();
    if !header_present || !lib_present {
        let mut missing = Vec::new();
        if !header_present {
            missing.push(format!("simavr headers (looked for {header})"));
        }
        if !lib_present {
            missing.push(format!("libsimavr.a (looked for {static_lib})"));
        }
        panic!(
            "\n\
             hauksbee-mcu: the `avr` co-sim feature needs a system libsimavr, but \
             couldn't find {missing}.\n\
             \n\
             simavr is GPL-3.0 and this repo is Apache-2.0, so simavr is NOT vendored here; \
             it is linked from your system by deliberate choice. To build the AVR \
             backend, pick one:\n\
             \n\
             1. Install simavr with one command:\n\
             \x20      scripts/install-sims.sh --avr\n\
             \n\
             2. Point the build at an existing install:\n\
             \x20      SIMAVR_INCLUDE_DIR=<prefix>/include SIMAVR_LIB_DIR=<prefix>/lib cargo build ...\n\
             \n\
             3. Build without AVR (no simavr needed; Renode + QEMU backends only):\n\
             \x20      cargo build -p hauksbee-engine --no-default-features --features renode,qemu\n",
            missing = missing.join(" and "),
        );
    }

    // Link against system-installed simavr (static)
    println!("cargo:rustc-link-search=native={lib_dir}");
    println!("cargo:rustc-link-lib=static=simavr");

    // Also need libelf and zlib (simavr dependencies). libsimavr itself is
    // linked STATIC above; if these two link dynamically, the installed
    // binary exits 127 with "libelf.so.1: cannot open shared object file" on
    // any distro without libelf1 (debian:bookworm-slim, ubuntu:24.04). Prefer
    // the static archives; fall back to dynamic with a printed note when the
    // build host carries no .a (the release builders must).
    probe_static_preferred(
        "libelf",
        "hauksbee-mcu: the `avr` feature needs libelf, but pkg-config can't find it. \
         Install it with `scripts/install-sims.sh --avr` (installs libelf too), or by hand: \
         `brew install libelf` (macOS) / `apt-get install libelf-dev` (Debian/Ubuntu) / \
         `dnf install elfutils-libelf-devel` (Fedora). To build without AVR: \
         `cargo build -p hauksbee-engine --no-default-features --features renode,qemu`.",
    );
    probe_static_preferred(
        "zlib",
        "hauksbee-mcu: the `avr` feature needs zlib, but pkg-config can't find it. \
         Install it: `brew install zlib` (macOS) / `apt-get install zlib1g-dev` (Debian/Ubuntu) / \
         `dnf install zlib-devel` (Fedora). To build without AVR: \
         `cargo build -p hauksbee-engine --no-default-features --features renode,qemu`.",
    );

    // Generate Rust bindings for simavr headers
    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_arg(format!("-I{include_dir}"))
        .clang_arg(format!("-I{include_dir}/simavr"))
        .allowlist_function("avr_.*")
        .allowlist_function("read_ihex_.*")
        .allowlist_function("elf_.*")
        .allowlist_type("avr_.*")
        .allowlist_type("elf_firmware_t")
        .allowlist_var("AVR_.*")
        .allowlist_var("cpu_.*")
        // The ioport IRQ enum (IOPORT_IRQ_REG_PORT etc.) MUST come from the
        // platform's own simavr header: its index SHIFTS between simavr
        // versions, e.g. the addition of IOPORT_IRQ_PIN_ALL_IN moves
        // IOPORT_IRQ_REG_PORT from 10 to 11. Hardcoding the index makes the
        // GPIO read subscribe to the wrong IRQ on a different simavr build, so
        // the port reads as "never driven" (GREEN on one host, RED on another).
        // Emit the real value via bindgen so it tracks whatever simavr is linked.
        .allowlist_item("IOPORT_.*")
        .allowlist_var("IOPORT_.*")
        // The UART IRQ enum is sourced from the linked simavr for the same
        // version-skew reason as IOPORT above. UART_IRQ_OUT_XON/XOFF are the
        // emulator's own RX flow control (its input fifo is only 64 bytes);
        // the AVR backend subscribes to them so host serial records longer
        // than the fifo are metered in instead of silently truncated.
        .allowlist_item("UART_.*")
        .allowlist_var("UART_.*")
        .generate()
        .expect("Unable to generate simavr bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("simavr_bindings.rs"))
        .expect("Couldn't write bindings");
}

/// Probe a pkg-config library preferring the STATIC archive; when no static
/// archive exists on the build host, fall back to the dynamic probe and say
/// so, rather than fail a dev build over a packaging concern.
fn probe_static_preferred(name: &str, missing_msg: &str) {
    // Probe WITHOUT emitting cargo metadata first: static vs dynamic is
    // decided below, only after checking the archive really exists on disk.
    let probe = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe(name)
        .expect(missing_msg);

    if cfg!(target_os = "macos") {
        // macOS (Homebrew): the proven path, keep it. Homebrew prefixes such
        // as /opt/homebrew/opt/libelf/lib are NOT in the pkg-config crate's
        // system_roots, so its statik(true) mode genuinely emits `static=`
        // there, and the Homebrew .pc files always publish -L so the archive
        // shows up in link_paths for the existence check.
        let has_static = probe.link_paths.iter().any(|dir| {
            probe
                .libs
                .iter()
                .any(|l| dir.join(format!("lib{l}.a")).exists())
        });
        if has_static {
            let _ = pkg_config::Config::new().statik(true).probe(name);
        } else {
            let _ = pkg_config::probe_library(name);
            let lib = probe.libs.first().map(String::as_str).unwrap_or(name);
            print_dynamic_warning(name, lib);
        }
        return;
    }

    // Linux (and other non-mac hosts): pkg_config's statik(true) mode can
    // NEVER static-link a /usr-rooted archive. The crate's
    // is_static_available() treats system_roots=[/usr] as "no static lib
    // here" and silently emits a dynamic `-lelf` instead, so the old code
    // took the "static" branch, suppressed the dynamic-link warning, and
    // still produced a binary NEEDING libelf.so.1. Emit the link directives
    // ourselves, and claim static only for an archive we have actually seen
    // at the exact directory we emit.
    //
    // Search ladder: pkg-config's own -L paths (empty for system dirs, which
    // is exactly why statik failed), then the package's real libdir asked
    // straight from pkg-config, then the conventional multiarch locations.
    let mut search_dirs: Vec<PathBuf> = probe.link_paths.clone();
    let pkg_config_bin = env::var("PKG_CONFIG").unwrap_or_else(|_| "pkg-config".to_string());
    if let Ok(out) = std::process::Command::new(&pkg_config_bin)
        .args(["--variable=libdir", name])
        .output()
    {
        if out.status.success() {
            let libdir = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !libdir.is_empty() {
                search_dirs.push(PathBuf::from(libdir));
            }
        }
    }
    for dir in [
        "/usr/lib/x86_64-linux-gnu",
        "/usr/lib/aarch64-linux-gnu",
        "/usr/lib64",
        "/usr/lib",
        "/usr/local/lib",
    ] {
        search_dirs.push(PathBuf::from(dir));
    }

    for lib in &probe.libs {
        let archive = format!("lib{lib}.a");
        if let Some(dir) = search_dirs.iter().find(|d| d.join(&archive).exists()) {
            println!("cargo:rustc-link-search=native={}", dir.display());
            println!("cargo:rustc-link-lib=static={lib}");
        } else {
            // No archive anywhere on the ladder: link dynamically and SAY so.
            // Never print nothing here; a silent dynamic link is exactly the
            // false confidence this function used to produce.
            for dir in &probe.link_paths {
                println!("cargo:rustc-link-search=native={}", dir.display());
            }
            println!("cargo:rustc-link-lib={lib}");
            print_dynamic_warning(name, lib);
        }
    }
}

/// The honest "you are getting a dynamic link" note, shared by every fallback
/// path so no branch can quietly link dynamic without it.
fn print_dynamic_warning(name: &str, lib: &str) {
    println!(
        "cargo:warning=hauksbee-mcu: no static {name} archive (lib{lib}.a) found on this build \
         host; linking {lib} dynamically. The installed binary will need the {name} shared \
         library on the target machine; release builders should install the static package."
    );
}
