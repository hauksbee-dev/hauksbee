use std::env;
use std::path::PathBuf;

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

    // Link against system-installed simavr (static)
    println!("cargo:rustc-link-search=native={lib_dir}");
    println!("cargo:rustc-link-lib=static=simavr");

    // Also need libelf and zlib (simavr dependencies)
    pkg_config::probe_library("libelf").expect("libelf not found — install elfutils or libelf-dev");
    pkg_config::probe_library("zlib").expect("zlib not found");

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
        // versions — e.g. the addition of IOPORT_IRQ_PIN_ALL_IN moves
        // IOPORT_IRQ_REG_PORT from 10 to 11. Hardcoding the index makes the
        // GPIO read subscribe to the wrong IRQ on a different simavr build, so
        // the port reads as "never driven" (GREEN on one host, RED on another).
        // Emit the real value via bindgen so it tracks whatever simavr is linked.
        .allowlist_item("IOPORT_.*")
        .allowlist_var("IOPORT_.*")
        .generate()
        .expect("Unable to generate simavr bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("simavr_bindings.rs"))
        .expect("Couldn't write bindings");
}
