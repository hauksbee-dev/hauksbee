# EasyEDA fixtures

Real boards exported from EasyEDA, committed unmodified, used by
`crates/hauksbee-extract/tests/easyeda.rs` and the engine's
`tests/easyeda_inputs.rs`. Both are public MIT-licensed hobby projects; neither
comes from Tarski or any private source. Each file is byte-for-byte what the
upstream repository serves at the pinned commit (verify with `shasum -a 256`).

| Fixture | Edition | Upstream file | sha256 |
|---|---|---|---|
| `rpitx-coax-nofilter.json` | Standard 6.4.17, docType 3 | https://raw.githubusercontent.com/IgrikXD/rpitx-coax-pcb/40582c0b6ff2594afade1b990690f4fa77cd2864/EasyEDA/1-PCB_PCB_rpitx-coax-pcb-NOFILTER.json | `7a523e99a258c232849d458a019ebc93313a2e344f0861fd79940394537429ea` |
| `rpitx-coax-gp1212-gp731.json` | Standard 6.4.17, docType 3 | https://raw.githubusercontent.com/IgrikXD/rpitx-coax-pcb/40582c0b6ff2594afade1b990690f4fa77cd2864/EasyEDA/2-PCB_PCB_rpitx-coax-pcb-GP1212-GP731.json | `063522b89fb40dd809d9b39ac53e5ad9ce383ebe9a2ff98eb63062e75fb51a0d` |
| `nano-hub-pcb1.epcb` | Pro 2.2.40.3, JSON Lines | https://raw.githubusercontent.com/Irtaza2009/Nano_Hub/9db0d3f031359fa0600c222858cd8ef9082cf32b/pcb/PCB1.epcb | `2b1546a865ee2af6edd148e342bcddc23462530181af1d411d2aeee65dfd48a1` |
| `nano-hub-bom.csv` | JLCPCB BOM export (tab-separated) | https://raw.githubusercontent.com/Irtaza2009/Nano_Hub/9db0d3f031359fa0600c222858cd8ef9082cf32b/production/BOM_Board1_PCB1_2025-07-22.csv | `63850d650b97ba9f5d4689290605bd035e3546d423bcdee0a59d2373ece9262c` (the upstream file is UTF-16LE, sha256 `d25806e28c113ffb041956205c5049ed5ca186610fd08e0fa5fb9ccb531a1330`; this copy is the same text transcoded to UTF-8, nothing else changed) |

Project pages: https://github.com/IgrikXD/rpitx-coax-pcb (Raspberry Pi GPIO to
SMA expansion board, 4 parts) and https://github.com/Irtaza2009/Nano_Hub (a
4-port USB hub board, 23 parts).

## Licences

Both repositories declare the MIT licence in their root `LICENSE` file. The text
below is copied from those files at the pinned commits (sha256 of the upstream
`LICENSE` files: rpitx-coax-pcb `f13ce8fff23e90043dcc9be55b08b80f7d6f305bc7032484a540fc85d00f08c6`,
Nano_Hub `1139e7d6a67b646c77ae3083c57622177aed01ed99e71b6923cfef2f9258b492`).

### rpitx-coax-pcb (the two `.json` boards)

```
MIT License

Copyright (c) 2021 Ihar Yatsevich

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

### Nano_Hub (the `.epcb` board and its BOM)

```
MIT License

Copyright (c) 2025 Muhammad Irtaza

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## What is and is not covered

* There is no EasyEDA Pro `.epro` project archive in an openly licensed public
  repository that was found, so the archive path is tested with a zip built in
  the test from the Pro fixture plus a hand-written `project.json`. That
  `project.json` layout is an inference, not a verified sample. The `devices`
  map (device UUID to `attributes` with `Value`, `Manufacturer Part`,
  `Manufacturer`, `Supplier Part`) that the reader uses to resolve a Pro part's
  value is inferred the same way, from public unzipped project exports, and is
  covered by in-test archives only.
* A Pro `.epcb` does not carry part values (its `Device` attribute is a UUID
  into the project's device library). `nano-hub-bom.csv` is the project's own
  BOM, used to test that `--bom` supplies the values the board file lacks.
* There is no KiCad export of either board upstream, so "the same result as the
  KiCad equivalent" is checked against a hand-curated known answer in the test
  (parts, pad counts and nets), not against a converter's output.
