all:
	avr-gcc -mmcu=atmega328p -DF_CPU=16000000UL -Os -Wall -o bitbang_spi_imu.elf main.c
	avr-objcopy -O ihex bitbang_spi_imu.elf bitbang_spi_imu.hex
