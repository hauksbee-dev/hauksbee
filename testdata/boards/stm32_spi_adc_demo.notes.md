# stm32_spi_adc_demo.kicad_pcb: design notes

These notes used to sit inside the board file as `;` comment lines.
KiCad's board format has no comment syntax, so its parser refused the
file and `kicad-cli` could not export gerbers from it. The reasoning is
worth keeping, so it lives here instead.

STM32F103C8 (LQFP-48) with SPI1 master wired to an MCP3008 ADC and a GPIO
indicator. Package-faithful pad map (ST DS5319):
pad 14 = PA4 = SPI1_NSS
pad 15 = PA5 = SPI1_SCK
pad 16 = PA6 = SPI1_MISO
pad 17 = PA7 = SPI1_MOSI
pad 29 = PA8 = FLAG (threshold output the co-sim test reads)
The hauksbee Mcp3008 slave is attached via SpiBus::attach_spi_bus in the
test; the MCP3008 is intercepted at the SPI-peripheral level (the Renode C#
bridge), NOT through these board nets, so the NSS/SCK/MISO/MOSI nets are for
physical completeness only. Only the PA8/FLAG mapping is load-bearing: it
routes the firmware's PA8 GPIO output to the "FLAG" net the test samples.

PA7/MOSI sits on pad 17. The shared STM32 model maps pad 17 -> PA8 because
the (frozen) I2C thermostat test board puts its FLAG net there; mapping the
MOSI net to pad 17 on THIS board would therefore put a second PA8 driver on
the MOSI net and race the FLAG driver (the binder's role->net map is
last-write-wins). Since MOSI is decorative here, it is left on the net list
without a U1 pad rather than introduce that collision. FLAG uses the real
PA8 pad (29), which is unambiguous.

FLAG -> 10k pulldown to ground so the output net has a resolved DC level
the solver can compute each chunk (same pattern as the blinky demo's PC13).
