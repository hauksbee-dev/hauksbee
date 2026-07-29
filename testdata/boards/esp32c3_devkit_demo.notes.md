# esp32c3_devkit_demo.kicad_pcb: design notes

These notes used to sit inside the board file as `;` comment lines.
KiCad's board format has no comment syntax, so its parser refused the
file and `kicad-cli` could not export gerbers from it. The reasoning is
worth keeping, so it lives here instead.

ESP32-C3 (RISC-V) devkit demo, mirroring the ESP32 (Xtensa) demo so the QEMU
riscv32 backend is exercised against the identical co-sim contract. GPIO2
drives an LED through a real 330R resistor (the analog current path the MNA
solver computes); GPIO4 drives a 4k7 to ground for the observable blink.
UART0 (GPIO21 TX / GPIO20 RX) is the serial bridge. The part runs at 3.3 V.
