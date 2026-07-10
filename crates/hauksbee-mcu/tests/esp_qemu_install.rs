//! The `qemu::install` acceptance boundary, tested without any network:
//! archives are built locally (the same top-level `qemu/` layout the
//! Espressif release tarballs carry) and the "binary" is a shell script, so
//! the post-unpack `is_esp_fork` verification runs against a controlled
//! machine list. The network half (release/tag/asset resolution) is covered
//! by the unit tests in `qemu/install.rs`, pinned to the live release's
//! published names; the full download path additionally re-runs the same
//! `unpack_archive` + `verify_installed` exercised here.

#![cfg(all(feature = "qemu", unix))]

use hauksbee_mcu::qemu::install::{unpack_archive, verify_installed};
use hauksbee_mcu::qemu::QemuArch;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

/// Build a release-shaped archive: `qemu/bin/<binary>` where the binary is a
/// script whose `-machine help` output is `machines`.
fn fake_release_archive(dir: &Path, binary: &str, machines: &str) -> std::path::PathBuf {
    let tree = dir.join("tree");
    let bin_dir = tree.join("qemu/bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let bin = bin_dir.join(binary);
    std::fs::write(
        &bin,
        format!("#!/bin/sh\nprintf '%s\\n' \"{machines}\"\n"),
    )
    .unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

    let archive = dir.join("fake-release.tar.gz");
    let status = Command::new("tar")
        .arg("czf")
        .arg(&archive)
        .arg("-C")
        .arg(&tree)
        .arg("qemu")
        .status()
        .expect("tar available");
    assert!(status.success(), "building the fixture archive failed");
    archive
}

#[test]
fn unpacked_esp_fork_binary_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let archive = fake_release_archive(
        dir.path(),
        "qemu-system-xtensa",
        "esp32                Espressif ESP32 machine\nesp32s3              Espressif ESP32S3 machine",
    );

    let root = dir.path().join("install-root");
    unpack_archive(&archive, &root).expect("unpack succeeds");
    let bin = verify_installed(QemuArch::Xtensa, &root).expect("fork binary accepted");
    assert_eq!(bin, root.join("qemu/bin/qemu-system-xtensa"));
}

#[test]
fn mainline_binary_without_esp32_machines_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    // A mainline qemu-system-xtensa advertises boards like lx60, sim — no esp32.
    let archive = fake_release_archive(
        dir.path(),
        "qemu-system-xtensa",
        "lx60                 lx60 board\nsim                  simulator",
    );

    let root = dir.path().join("install-root");
    unpack_archive(&archive, &root).expect("unpack succeeds");
    let err = verify_installed(QemuArch::Xtensa, &root).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("machine-list check failed"),
        "rejection must name the fork check: {msg}"
    );
}

#[test]
fn missing_binary_after_unpack_is_a_loud_layout_error() {
    let dir = tempfile::tempdir().unwrap();
    // Archive carries only the xtensa binary; verifying riscv32 must fail
    // with the expected path in the message, not a panic or a silent pass.
    let archive = fake_release_archive(
        dir.path(),
        "qemu-system-xtensa",
        "esp32                Espressif ESP32 machine",
    );
    let root = dir.path().join("install-root");
    unpack_archive(&archive, &root).expect("unpack succeeds");
    let err = verify_installed(QemuArch::Riscv32, &root).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("qemu-system-riscv32") && msg.contains("missing after unpack"),
        "layout error must name the missing binary: {msg}"
    );
}
