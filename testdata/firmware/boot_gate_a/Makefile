all:
	avr-gcc -mmcu=atmega328p -DF_CPU=16000000UL -Os -Wall -o boot_gate.elf main.c
	avr-objcopy -O ihex boot_gate.elf boot_gate.hex
