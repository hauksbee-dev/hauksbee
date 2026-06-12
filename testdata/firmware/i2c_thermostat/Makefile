all:
	avr-gcc -mmcu=atmega328p -DF_CPU=16000000UL -Os -Wall -o thermostat.elf main.c
	avr-objcopy -O ihex thermostat.elf thermostat.hex
