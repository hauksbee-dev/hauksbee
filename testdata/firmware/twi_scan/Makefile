# Raw-assembly build: needs only GNU binutils-avr (no avr-gcc / avr-libc).
all:
	avr-as -mmcu=atmega328p main.S -o main.o
	avr-ld -m avr5 -Ttext=0 -e _start main.o -o scan.elf
	avr-objcopy -O ihex scan.elf scan.hex
	rm -f main.o

clean:
	rm -f main.o scan.elf scan.hex
