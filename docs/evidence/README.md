# Evidence

- [`KNOWN_FAULTS_VALIDATION.md`](KNOWN_FAULTS_VALIDATION.md): calibration against faults documented in real boards' revision histories; eight in-scope faults, six caught statically, one executed via co-sim, one honest miss.
- [`BUG_HUNT.md`](BUG_HUNT.md): the hunt that found a real, previously-uncaught catastrophic miswire on a 3,442-component production board, plus the cold re-derivation of the Raspberry Pi 4 USB-C fault.
- [`FAMOUS_SWEEP.md`](FAMOUS_SWEEP.md): five sweep rounds over famous open-hardware boards, every candidate chased to the file level, with an honest clean as the verdict.

The discipline behind all three: a check earns its place by being shown not to
fire on known-good shipped boards before it lands, and misses are reported with
the reason rather than hidden.

Where that discipline currently stops is written down too, in `FAMOUS_SWEEP.md`:
one live false positive on the Arduino Uno's `RESET-EN` solder jumper, no single
corpus-wide silence gate over the whole lint, and several corpus guards that
address boards by a layout `scripts/fetch-corpus.sh` does not produce. Evidence
documents that only recorded their wins would not be evidence.
