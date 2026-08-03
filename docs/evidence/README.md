# Evidence

- [`KNOWN_FAULTS_VALIDATION.md`](KNOWN_FAULTS_VALIDATION.md): calibration against faults documented in real boards' revision histories; eight in-scope faults, six caught statically, one executed via co-sim, one honest miss.
- [`BUG_HUNT.md`](BUG_HUNT.md): the hunt that found a real, previously-uncaught catastrophic miswire on a 3,442-component production board, plus the cold re-derivation of the Raspberry Pi 4 USB-C fault.
- [`FAMOUS_SWEEP.md`](FAMOUS_SWEEP.md): five sweep rounds over famous open-hardware boards, every candidate chased to the file level, with an honest clean as the verdict.

The discipline behind all three: checks are tuned to zero false positives on known-good shipped boards before they land, and misses are reported with the reason rather than hidden.
