# esp32_spi_adc_demo.kicad_pcb: design notes

These notes used to sit inside the board file as `;` comment lines.
KiCad's board format has no comment syntax, so its parser refused the
file and `kicad-cli` could not export gerbers from it. The reasoning is
worth keeping, so it lives here instead.

ESP32-WROOM-32 with HSPI (SPI2) master wired to an MCP3008 ADC and a GPIO
indicator. HSPI: SCLK=GPIO14 (pad 14), MISO=GPIO12 (pad 14 -- see model),
MOSI=GPIO13 (pad 18 not modeled), CS=GPIO15 (pad 23). The SPI nets are for
physical traceability; interception is at the SPI peripheral level (not yet
wired for QEMU backend).
GPIO4 (pad 26, role "p04") -> net "FLAG": the observable threshold output.
Pad numbering per KiCad RF_Module:ESP32-WROOM-32.

FLAG -> 10k pulldown to ground so the output net has a resolved DC level
the solver can compute each chunk.
