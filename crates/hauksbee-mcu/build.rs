use std::env;
use std::path::{Path, PathBuf};

fn main() {
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

    // Also need libelf and zlib (simavr dependencies)
    pkg_config::probe_library("libelf").expect(
        "hauksbee-mcu: the `avr` feature needs libelf, but pkg-config can't find it. \
         Install it with `scripts/install-sims.sh --avr` (installs libelf too), or by hand: \
         `brew install libelf` (macOS) / `apt-get install libelf-dev` (Debian/Ubuntu) / \
         `dnf install elfutils-libelf-devel` (Fedora). To build without AVR: \
         `cargo build -p hauksbee-engine --no-default-features --features renode,qemu`.",
    );
    pkg_config::probe_library("zlib").expect(
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
