all:
	avr-gcc -mmcu=atmega328p -nostdlib -Wl,--section-start=.text=0,-e,_start main.S -o atomic.elf
	avr-objcopy -O ihex atomic.elf atomic.hex
	perl -pi -e 's/\r\n/\n/g' atomic.hex
