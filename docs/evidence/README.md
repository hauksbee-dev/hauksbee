# Evidence

- [`KNOWN_FAULTS_VALIDATION.md`](KNOWN_FAULTS_VALIDATION.md): calibration against faults documented in real boards' revision histories; eight in-scope faults, six caught statically, one executed via co-sim, one honest miss. It also carries the adjudications of copper shorts hauksbee raised live on boards nobody had filed a fault against, where the ground truth is a sibling layout, a second tool's DRC, or the board's own fabrication output: two real (ODrive v2 attempt, MWGEN-G1) and one declared emonTx V3.4.5 net tie, now read from its same-design Eagle schematic companion.
- [`BUG_HUNT.md`](BUG_HUNT.md): the hunt that found a real, previously-uncaught catastrophic miswire on a 3,442-component production board, plus the cold re-derivation of the Raspberry Pi 4 USB-C fault.
- [`FAMOUS_SWEEP.md`](FAMOUS_SWEEP.md): five sweep rounds over famous open-hardware boards, every candidate chased to the file level, with an honest clean as the verdict.

The discipline behind all three: a check earns its place by being shown not to
fire on known-good shipped boards before it lands, and misses are reported with
the reason rather than hidden.

Where that discipline stops is written down too, because evidence documents that
only recorded their wins would not be evidence. `FAMOUS_SWEEP.md` carries: the
false positive this calibration had to kill (the Arduino Uno's `RESET-EN` solder
jumper, read as an unfinished BOM entry); the full-corpus lint tally, in which one
finding on the MNT Reform 2.0/2.5 DAC bus remains unadjudicated; a clearance gap of
9.77e-15 mm that is touching copper and is classified as a clearance note only
because the short test is a strict `gap <= 0.0`; and one sweep row whose clean
shorts result comes from a KiCad 10 file the tool marks UNRELIABLE.

[`CORPUS.md`](CORPUS.md) carries the same discipline applied to the corpus itself:
what the fetch lands, which gate reads which entry, which entries feed no gate, and
the findings that surfaced on boards added to it.
