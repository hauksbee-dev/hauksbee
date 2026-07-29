# esp32_devkit_demo.kicad_pcb: design notes

These notes used to sit inside the board file as `;` comment lines.
KiCad's board format has no comment syntax, so its parser refused the
file and `kicad-cli` could not export gerbers from it. The reasoning is
worth keeping, so it lives here instead.

ESP32-WROOM-32 module demo, mirroring the STM32 blue pill demo so the QEMU
backend is exercised against the identical co-sim contract. GPIO2 drives an
LED through a real 330R resistor (the analog current path the MNA solver
computes when the firmware drives GPIO2 HIGH at boot); GPIO4 drives a 4k7 to
ground so the ~5 Hz blink is observable in the solved circuit. UART0
(U0TXD/U0RXD) is the serial bridge. The part runs at 3.3 V.

GPIO2 -> 330R -> LED anode. This resistor's current is what the solver must
compute when GPIO2 is driven HIGH (3.3 V) at boot.

GPIO4 -> 4k7 pulldown to ground so the blink net sits at a clean logic level
the solver resolves each chunk.
