# Security policy

## Reporting a vulnerability

Please report security issues privately rather than in a public issue:
through GitHub's
[private vulnerability reporting](https://github.com/hauksbee-dev/hauksbee/security/advisories/new),
or by email to `security@hauksbee.dev`.

Include what you did, what happened, and a minimal redacted reproduction when
one can be shared safely. Do not send proprietary board files, firmware, or
specifications until a confidential channel and handling terms are agreed.

We aim to acknowledge reports within a few days. This is a small project, so
please be patient with fixes.

## What is in scope

The parts of hauksbee that handle input you did not write are the ones worth
attacking:

- **`hauksbee serve`**, the web front door. It accepts uploaded board files,
  firmware images, and check specs from a browser. Anything that gets it to read
  or write outside its temporary directory, run a command, exhaust memory or
  disk on a modest upload, or crash the process is in scope.
- **The board extractors** (`hauksbee-extract`): KiCad, Eagle, Altium, IPC-D-356,
  and gerber parsing. These read untrusted binary and text.
- **Firmware ingestion** (`hauksbee-engine/src/firmware_input.rs`), including
  zip extraction and the PlatformIO build path, which can invoke a toolchain.
- **The CI spec parser** (`hauksbee-ci`), since specs arrive from repositories.

A panic on malformed input is a bug worth reporting, not just a crash: it is a
denial of service when it happens inside `serve`.

## What is out of scope

- `hauksbee serve` binds to localhost and is designed for a trusted single user
  on their own machine. It has no authentication and is not hardened for
  deployment on a public address. Exposing it to a network is outside its
  intended use, and issues that depend on doing so are not vulnerabilities in
  hauksbee. If you need a shared deployment, put it behind your own
  authenticating proxy.
- Wrong simulation results are correctness bugs, not security issues. Please do
  open an issue for them, with the board.
- Vulnerabilities in the optional emulator backends (simavr, Renode, QEMU)
  belong upstream, though we would like to know if hauksbee's use of them makes
  something exploitable that would not otherwise be.

## Supported versions

hauksbee is pre-1.0 and only the latest release receives fixes.
