use std::env;
use std::path::PathBuf;

fn main() {
    // The simavr link + bindgen step is only needed for the `avr` backend.
    // When that feature is off (e.g. a renode-only build on a machine without
    // libsimavr), skip it entirely so the crate still compiles.
    if env::var_os("CARGO_FEATURE_AVR").is_none() {
        return;
    }

    // Link against system-installed simavr (homebrew on macOS)
    println!("cargo:rustc-link-search=/opt/homebrew/lib");
    println!("cargo:rustc-link-lib=static=simavr");

    // Also need libelf and zlib (simavr dependencies)
    pkg_config::probe_library("libelf").expect("libelf not found — install elfutils or libelf-dev");
    pkg_config::probe_library("zlib").expect("zlib not found");

    // Generate Rust bindings for simavr headers
    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_arg("-I/opt/homebrew/include")
        .clang_arg("-I/opt/homebrew/include/simavr")
        .allowlist_function("avr_.*")
        .allowlist_function("read_ihex_.*")
        .allowlist_function("elf_.*")
        .allowlist_type("avr_.*")
        .allowlist_type("elf_firmware_t")
        .allowlist_var("AVR_.*")
        .allowlist_var("cpu_.*")
        .generate()
        .expect("Unable to generate simavr bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("simavr_bindings.rs"))
        .expect("Couldn't write bindings");
}
