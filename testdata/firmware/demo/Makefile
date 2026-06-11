all:
	avr-gcc -mmcu=atmega328p -DF_CPU=16000000UL -Os -Wall -o demo.elf main.c
	avr-objcopy -O ihex demo.elf demo.hex
