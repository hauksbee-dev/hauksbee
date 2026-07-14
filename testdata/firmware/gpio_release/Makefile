# Raw-assembly build: needs only GNU binutils-avr (no avr-gcc / avr-libc).
all:
	avr-as -mmcu=atmega328p main.S -o main.o
	avr-ld -m avr5 -Ttext=0 -e _start main.o -o release.elf
	avr-objcopy -O ihex release.elf release.hex
	rm -f main.o

clean:
	rm -f main.o release.elf release.hex
