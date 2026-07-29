# stm32_adc_divider_demo.kicad_pcb: design notes

These notes used to sit inside the board file as `;` comment lines.
KiCad's board format has no comment syntax, so its parser refused the
file and `kicad-cli` could not export gerbers from it. The reasoning is
worth keeping, so it lives here instead.

The blue pill demo board PLUS an analog input: a 10k/10k divider from +3V3
feeds TEMP_SENSE into PA0 (LQFP-48 pin 10, role pa0_adc0 -> engine ADC
channel 0). The scheduler pushes the solved TEMP_SENSE voltage into the
core every chunk; the stock Renode STM32F103 platform models NO ADC, so
the backend records the injection as DROPPED — this board is the live
fixture for the co-sim ADC-coverage honesty surfaces (U3 finding 1).

PA5 -> 330R -> LED anode (the observable analog path from the base demo).

PC13 -> 4k7 pulldown so the blink net sits at a clean logic level.

+3V3 -> 10k -> TEMP_SENSE -> 10k -> GND: the divider that puts ~1.65 V on
the ADC net, standing in for an analog sensor output (an LM35-style part).
