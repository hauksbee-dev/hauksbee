# Hauksbee beta

Hauksbee is pre-1.0 engineering software. The beta is for finding where the
product is useful, wrong, or short of evidence. It does not replace design
review, CAD DRC, vendor limits, compliance testing, or hardware measurements.

The beta is distributed as SemVer pre-release tags on this repository. Each
tag names one source commit, and every downloadable asset carries a neighbouring
SHA-256 manifest. Hauksbee's own source is Apache-2.0 under `LICENSE`; the
binaries are labelled individually, because the default download statically
links a GPL-3.0 component and the `-permissive` one does not.
[COMPLIANCE](COMPLIANCE.md) states the licence of each artifact in one row, and
[release and licensing](docs/about/release-and-licensing.md) gives the reasoning.
Third-party components keep their own licences.

## The useful beta loop

1. Start with a board whose behaviour you understand. Keep your normal review
   and simulation flow in place.
2. Drop the board, schematic, or fab archive into the local web app. Add the
   BOM, placement, fitted variant, as-built overlay, firmware, model files, or
   assertion spec when they determine the answer. The same selected design
   bundle is used by the report, Checks, and Live Sim.
3. Check the input inventory and model-coverage sections before reading the
   verdict. A clean result with missing authority is not a clean board.
4. Reproduce any important finding in the source CAD, an independent
   calculation, another simulator, or on the bench before changing hardware.
5. Send the exact Hauksbee version and the exported report with feedback. The
   version includes the source commit; the report records its inputs and
   evidence.

During beta, upgrade to the newest build before reporting a bug. Only the latest beta receives fixes.

## Getting a beta build

Take the newest pre-release tag from the
[releases page](https://github.com/hauksbee-dev/hauksbee/releases) and install
that exact tag:

```bash
curl -fsSL https://raw.githubusercontent.com/hauksbee-dev/hauksbee/main/scripts/get-hauksbee.sh \
  | bash -s -- --version <the tag>
```

Add `--permissive` for the Apache-2.0 binary shape. No credential is needed:
the releases are public. The installer verifies the asset digest and refuses a
release whose assets are replaceable, but check for yourself that the source
commit the binary reports is the one the tag names.

Do not install a beta through a moving `latest` URL, and never pin one for a
hardware gate. There is no stable channel yet, so the exact tag is the only
correct source.

## Privacy

Ordinary board analysis runs locally. Hauksbee does not upload a board merely because it was dropped into the app.

The optional **Draft from datasheet** action is different. After a privacy/cost
notice and explicit consent, local agent backends read selected page images and
the sandboxed PDF/text; the API backend sends extracted datasheet text and
instructions to its configured endpoint. Do not use it with confidential
material unless your agreement with that backend permits it. Saving the draft
is a separate reviewed action.

Reports and board files are never attached to a GitHub issue automatically. Before sharing either, remove confidential paths, part numbers, firmware, and design data. For a security vulnerability, use [private vulnerability reporting](SECURITY.md) instead of a public issue.

## What to report

The highest-value reports are:

- a false positive on a board known to be correct;
- a real defect Hauksbee missed;
- a strong verdict produced from incomplete or contradictory evidence;
- a crash, hang, corrupt export, or flow that cannot be completed without a terminal;
- an error message that does not say what evidence or next action is needed;
- a result that changes between identical runs.

Use the repository's **Beta experience**, **Bug report**, or **False positive** issue form. If the board can be shared, attach the smallest failing input plus the fixed control. If it cannot, include the exported report after redaction, the output of `hauksbee --version`, the platform, and the smallest description that preserves the electrical topology involved.

## Platform status

- **macOS:** publish the `Hauksbee.app` zip only after same-source signing and notarisation pass.
- **Windows:** use only a release that includes the Windows asset and its retained native Windows gate evidence.
- **Linux:** use the architecture-matched tarball. The app-like entry point is `hauksbee serve`; desktop packaging remains platform-specific.
- **CI:** use a release-pinned binary. Do not pin a moving branch or mutable tag for a hardware gate.

Every downloadable asset has a neighbouring SHA-256 manifest. A beta release is accepted only when the downloaded bytes match that manifest and the binary reports the source commit named by the release.

The Altium `.PcbDoc` reader remains outside the stable-release decision
until its record-layout provenance review is resolved. Do not publish an
Apache-2.0 release containing that reader on the strength of an engineering
assessment alone; see [release and licensing](docs/about/release-and-licensing.md#7-provenance-of-the-altium-ingest).

The stable Hauksbee-owned core will be published under Apache-2.0 after its
technical and legal review is complete. That commitment grants no Apache-2.0
rights in this beta and promises no release date.

## Evidence language

Treat labels literally:

- source-bound static checks come from the identified design bytes and rules;
- simulation and emulator results are observations inside the named model and backend, not measured hardware;
- analytical oracles are calculations used for cross-checking;
- invalid, incomplete, or refused results are not green;
- a generated datasheet model is a draft until its pins, parameters, limitations, and positive/negative tests have been reviewed.

The current supported scope and known boundaries are maintained in [CAPABILITIES](docs/about/CAPABILITIES.md) and [LIMITATIONS](docs/about/LIMITATIONS.md).
