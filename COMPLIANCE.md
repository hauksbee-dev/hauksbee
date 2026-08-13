# Licence compliance

What licence applies to each thing hauksbee ships, and what each one obliges
you to do. One page, per artifact. The reasoning behind the split is in
[`docs/about/release-and-licensing.md`](docs/about/release-and-licensing.md);
this page is the answer without the reasoning.

| Artifact | Licence | Obligation if you redistribute |
|---|---|---|
| This source tree | Apache-2.0 | Retain `LICENSE` and `NOTICE`, state changes, keep attribution notices |
| `hauksbee-<ver>-source.tar.gz` | Mixed source licences as recorded per component | Complete same-release Corresponding Source for the default tarballs/app: exact Hauksbee, locked Cargo registry, and simavr trees |
| `hauksbee-<ver>-<target>.tar.gz` (default binary download) | GPL-3.0 with MIT dependency notice | Full GPL-3.0 terms, including offering corresponding source; retain `LICENSE-EVALEXPR-MIT.txt` |
| `hauksbee-<ver>-<target>-permissive.tar.gz` | Apache-2.0 with MIT dependency notice | Retain `LICENSE`, `NOTICE`, and `LICENSE-EVALEXPR-MIT.txt`; no GPL obligations |
| `Hauksbee.app` (macOS app zip) | GPL-3.0 with MIT dependency notice | Same as the default binary; retain `LICENSE-EVALEXPR-MIT.txt` |
| `hauksbee-<ver>-<target>-permissive-app.zip` (local builder only) | Apache-2.0 with MIT dependency notice | Retain `LICENSE`, `NOTICE`, and `LICENSE-EVALEXPR-MIT.txt`; no GPL obligations |
| `ghcr.io/hauksbee-dev/hauksbee:slim` | GPL-3.0-only with MIT dependency notice | Full GPL-3.0 terms; retain the enclosed notices and corresponding-source archives |
| `ghcr.io/hauksbee-dev/hauksbee:full` | GPL-3.0-only AND GPL-2.0-only AND MIT | Retain the component texts under `/usr/share/doc/hauksbee/`; GPL terms apply to the corresponding bundled components |
| Separately installed simulator backends | Renode: MIT; Espressif QEMU: GPL-2.0 | The tarballs/apps do not redistribute them; the full container does, with their exact license texts |

Running any of these imposes nothing. GPL-3.0 constrains distribution, not use.
Every obligation below is triggered by redistributing or embedding, not by
using the tool on your own boards.

**The source tree.** All of hauksbee's own code is Apache-2.0, including the
extraction, models, solver, checks, co-simulation coupling, CI runner, MCP
server, and the vendored `kicad-forge` crates under `vendor/kicad-forge`. The
[`NOTICE`](NOTICE) file must ride along with any redistribution, in source or
binary form, per Apache-2.0 section 4(d).

**The default binary download.** The default build includes the `avr`
co-simulation backend, which statically links libsimavr. libsimavr is GPL-3.0,
and statically linking GPL-3.0 code makes the resulting binary a combined work,
so the binary is GPL-3.0 even though the source stays Apache-2.0. If you
redistribute this download, or ship it inside a product, GPL-3.0 applies to
you, including the obligation to offer the corresponding source. The tarball
encloses the full GPL-3.0 text as `LICENSE-GPL-3.0.txt`, names the exact
Hauksbee and simavr commits, and points to the checksummed
`hauksbee-<ver>-source.tar.gz` asset published beside it. That source asset
contains the exact Hauksbee tree, every locked Cargo registry package compiled
into the binaries, and the exact simavr source tree.

**The permissive download.** The `-permissive` tarball is built with
`--no-default-features --features renode,qemu`. It contains no AVR backend, no
libsimavr, and no GPL code linked or embedded, so the binary is Apache-2.0.
This is the download to take if you are redistributing, repackaging, or
embedding hauksbee. It trades away AVR co-simulation for that. Release CI
also rejects AGPL runtime dependencies. The expression evaluator is pinned to
the final MIT line, evalexpr 11.3.1, and its exact notice travels as
`LICENSE-EVALEXPR-MIT.txt` in every binary artifact.
Release CI
verifies the shape behaviourally, by running the binary's own `doctor` output
and refusing to publish a build whose report contradicts its label, so a
feature-graph change cannot silently ship GPL code under the Apache-2.0 name.

**The macOS app.** The app published by release automation wraps the default
shape, so its zip is GPL-3.0 on the same reasoning as the default tarball. The
builder also exposes a distinctly named `-permissive-app.zip` local,
non-release app shape: it uses the same Apache-2.0 Renode/QEMU feature set as
the permissive tarball, contains no AVR/libsimavr backend, and carries no GPL
obligations. Release automation does not publish that local variant. Separate
from licensing: every published macOS release binary is signed with a
Developer ID identity; the app is notarised with the ticket stapled, and the
tarball binaries are notarised from launch onward, with Gatekeeper confirming
their ticket online on first run. Mechanics:
[`app/macos/SIGNING.md`](app/macos/SIGNING.md).

**The Docker images.** The slim image is labelled GPL-3.0-only because its
Hauksbee binaries are the default shape, statically linking libsimavr. The
`:full` image is a collection of separately licensed components: those same
binaries and freerouting under GPL-3.0, Espressif QEMU under GPL-2.0, and
Renode under MIT. Its OCI label enumerates that mixed set, and exact upstream
license texts are retained under `/usr/share/doc/hauksbee/third-party/` after
hash verification. The exact Hauksbee tree, complete locked Cargo registry
sources, and simavr tree used by the offline release build are enclosed under
`/usr/share/doc/hauksbee/source/`; the full image inherits those archives and
adds the exact-source references and notices for its separately bundled tools.
`SOURCE-OFFER.txt` also carries Hauksbee's three-year written offer. Pulling and
running an image is use, not distribution;
re-publishing it must preserve the applicable component terms and notices.

**`LICENSE-BINARY.txt`, the per-artifact contract.** Every release tarball
carries a `LICENSE-BINARY.txt` whose first line states the licence of the
binaries in that specific download: `UNDER GPL-3.0` or `UNDER APACHE-2.0`. It
also records the corresponding-source pointers required by GPL-3.0 section 6.
That file, not this page and not the repository `LICENSE`, is the authoritative
statement for a download you already have. Release CI greps each tarball for
its expected string before publishing, so the two shapes' labels cannot be
swapped.

**`NOTICE`.** [`NOTICE`](NOTICE) is the attribution mechanism required by
Apache-2.0 section 4(d), and it must be retained in redistributions of the
source and of the Apache-2.0 binary. It carries the copyright line, a
one-paragraph description of the product, and the KiCad attribution described
next.

**Altium record layouts, ported from KiCad.** The `.PcbDoc` reader's binary
record layouts were ported field-by-field from the record-layout descriptions
in KiCad's Altium importer, which is GPL-3.0. No KiCad code is copied into this
repository. The project's position is that on-disk field order, offsets, sizes,
and enum meanings are facts about Altium's format rather than KiCad's
copyrightable expression, which is why the reader ships in the Apache-2.0 core.
That position is a legal judgment rather than a settled question, it is stated
openly rather than buried, and `NOTICE` carries a KiCad attribution line either
way. The full statement, including what happens if a review concludes
otherwise, is section 7 of
[`docs/about/release-and-licensing.md`](docs/about/release-and-licensing.md),
and the "Records adapted from KiCad" section of
[`docs/ingest/ALTIUM.md`](docs/ingest/ALTIUM.md) names the exact source files.

**Contributions.** First-time contributors sign the contributor licence
agreement in [`CLA.md`](CLA.md). Contributors keep their copyright.

Questions this page does not answer, or a redistribution shape it does not
cover: open an issue, or use the contact in [`SECURITY.md`](SECURITY.md) if the
question is not one you want public.
