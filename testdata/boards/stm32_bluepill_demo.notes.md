# stm32_bluepill_demo.kicad_pcb: design notes

These notes used to sit inside the board file as `;` comment lines.
KiCad's board format has no comment syntax, so its parser refused the
file and `kicad-cli` could not export gerbers from it. The reasoning is
worth keeping, so it lives here instead.

STM32F103C8 "blue pill". PA5 drives an LED through a real 330R resistor
(the analog current path the solver computes); PC13 drives the onboard LED
net through its own resistor to ground so the blink is observable in the
solved circuit as well as via the GPIO bridge. USART1 (PA9/PA10) is the
serial bridge. The part runs at 3.3 V.

PA5 -> 330R -> LED anode. This resistor's current is what the solver must
compute when PA5 is driven HIGH (3.3 V) at boot.

PC13 -> 4k7 pulldown to ground so the blink net sits at a clean logic level
the solver resolves each chunk (the onboard LED on a real blue pill is
active-low to 3V3; here we keep a simple observable indicator).
