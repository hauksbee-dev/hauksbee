# OpenMower net-tie oracle

Re-run on 2026-08-07 against
[`xtech/hw-openmower-universal`](https://github.com/xtech/hw-openmower-universal)
commit `62dd369044c0bcecd5c6d7a50d026cd0edc651ff`.

Input:

- `hw-openmower-universal.kicad_pcb`
- SHA-256: `f7631145f52d126ff08d5ab69e8ba63123f8a888b0411eebfc26eaeefd9ee9bc`

KiCad command:

```text
/Applications/KiCad/KiCad.app/Contents/MacOS/kicad-cli pcb drc \
  --format json \
  --output openmower_62dd369_kicad_9_0_3_drc.json \
  hw-openmower-universal.kicad_pcb
```

KiCad 9.0.3 reported exactly two non-clearance violations
(`zones_intersect` and `courtyards_overlap`), zero clearance violations, and
zero unconnected items. The unedited generated report is committed beside this
file as `openmower_62dd369_kicad_9_0_3_drc.json`.

Hauksbee command:

```text
cargo run -p hauksbee-extract --example drc_probe -- \
  hw-openmower-universal.kicad_pcb 10
```

Exact summary:

```text
clearance_mm=0.2 primitives=225347 shorts=0 clearance_violations=0
short item-kind histogram: {}
--- first 10 shorts ---
```

This closes the former 114-warning false-positive gap without claiming that
KiCad's two unrelated board-level violations are absent.

The same `drc_probe` binary was then run over every committed sibling board in
`testdata/boards`. All nine remained silent:

```text
button_pullup.kicad_pcb              shorts=0 clearance_violations=0
esp32_devkit_demo.kicad_pcb          shorts=0 clearance_violations=0
esp32_spi_adc_demo.kicad_pcb         shorts=0 clearance_violations=0
esp32c3_devkit_demo.kicad_pcb        shorts=0 clearance_violations=0
stm32_adc_divider_demo.kicad_pcb     shorts=0 clearance_violations=0
stm32_bluepill_demo.kicad_pcb        shorts=0 clearance_violations=0
stm32_i2c_thermostat.kicad_pcb       shorts=0 clearance_violations=0
stm32_spi_adc_demo.kicad_pcb         shorts=0 clearance_violations=0
vcd_pulse.kicad_pcb                  shorts=0 clearance_violations=0
```
