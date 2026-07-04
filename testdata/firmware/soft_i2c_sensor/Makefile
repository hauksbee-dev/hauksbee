all:
	avr-gcc -mmcu=atmega328p -DF_CPU=16000000UL -Os -Wall -o soft_i2c_sensor.elf main.c
	avr-objcopy -O ihex soft_i2c_sensor.elf soft_i2c_sensor.hex
