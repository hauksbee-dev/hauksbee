# Licence compliance

What licence applies to each thing hauksbee ships, and what each one obliges
you to do. One page, per artifact. The reasoning behind the split is in
[`docs/about/release-and-licensing.md`](docs/about/release-and-licensing.md);
this page is the answer without the reasoning.

| Artifact | Licence | Obligation if you redistribute |
|---|---|---|
| This source tree | Apache-2.0 | Retain `LICENSE` and `NOTICE`, state changes, keep attribution notices |
| `hauksbee-<ver>-<target>.tar.gz` (default binary download) | GPL-3.0 | Full GPL-3.0 terms, including offering corresponding source |
| `hauksbee-<ver>-<target>-permissive.tar.gz` | Apache-2.0 | Retain `LICENSE` and `NOTICE`; no GPL obligations |
| `Hauksbee.app` (macOS app zip) | GPL-3.0 | Same as the default binary: the app contains the default shape |
| `ghcr.io/hauksbee-dev/hauksbee:slim` and `:full` | GPL-3.0-only | Full GPL-3.0 terms; the `:full` image also carries GPL-2.0 and GPL-3.0 tools |
| External simulator backends (Renode, Espressif QEMU) | MIT / GPL-2.0, not distributed by us | None from us: they are separately installed and run as separate processes |

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
encloses the full GPL-3.0 text as `LICENSE-GPL-3.0.txt` and names the exact
hauksbee commit and pinned simavr tag it was built from.

**The permissive download.** The `-permissive` tarball is built with
`--no-default-features --features renode,qemu`. It contains no AVR backend, no
libsimavr, and no GPL code linked or embedded, so the binary is Apache-2.0.
This is the download to take if you are redistributing, repackaging, or
embedding hauksbee. It trades away AVR co-simulation for that. Release CI
verifies the shape behaviourally, by running the binary's own `doctor` output
and refusing to publish a build whose report contradicts its label, so a
feature-graph change cannot silently ship GPL code under the Apache-2.0 name.

**The macOS app.** `Hauksbee.app` wraps the default shape, so the app zip is
GPL-3.0 on the same reasoning as the default tarball. There is no permissive
app shape. Separate from licensing: every macOS release binary is signed with
a Developer ID identity; the app is notarised with the ticket stapled, and
the tarball binaries are notarised from launch onward, with Gatekeeper
confirming their ticket online on first run. Mechanics:
[`app/macos/SIGNING.md`](app/macos/SIGNING.md).

**The Docker images.** Both published images are labelled
`org.opencontainers.image.licenses="GPL-3.0-only"`. The hauksbee binaries
inside are the default shape, statically linking libsimavr. The `:full` image
additionally bundles freerouting (GPL-3.0) and the Espressif QEMU fork
(GPL-2.0). Pulling and running an image is use, not distribution;
re-publishing an image derived from these carries the GPL-3.0 terms.

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
