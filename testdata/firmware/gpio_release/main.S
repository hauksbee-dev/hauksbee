; GPIO output-release firmware for the hauksbee AVR co-sim driver-release proof.
;
; Drives PC2 (Arduino Nano A2) as a plain GPIO output HIGH for ~28 ms, then
; hands the pin back: DDRC2 -> input while PORTC2 stays 1 (the open-drain
; "release = input with pull-up" idiom of a bus hand-off). Keeping PORTC2
; high on release is deliberate — it produces NO PORT edge, so the only
; signal the engine gets is the DDR direction change. A net with an external
; pull-down must then fall; before the driver-release fix the pin's Thevenin
; leg stayed enabled at 5 V and the net was latched high forever.
;
; Written in raw assembly (avr-as, no avr-libc) so the fixture rebuilds with
; just GNU binutils-avr; the committed .hex is what the tests load.
;
; ATmega328P @ 16 MHz.

	.section .text
	.global _start
_start:
	; Stack pointer -> RAMEND (0x08FF); no calls, kept for hygiene.
	ldi r16, 0x08
	sts 0x5E, r16           ; SPH
	ldi r16, 0xFF
	sts 0x5D, r16           ; SPL

	; PC2 output, driven HIGH.
	ldi r16, 0x04           ; bit 2
	out 0x07, r16           ; DDRC |= PC2 (output)
	out 0x08, r16           ; PORTC |= PC2 (high)

	; Hold ~28 ms: 3 * 200 * ~750 cycles at 16 MHz.
	ldi r20, 3
outer:
	ldi r19, 200
mid:
	ldi r18, 250
inner:
	dec r18
	brne inner
	dec r19
	brne mid
	dec r20
	brne outer

	; Release: direction back to input, PORT bit left HIGH (no PORT edge).
	ldi r16, 0x00
	out 0x07, r16           ; DDRC = 0 (all inputs)
halt:
	rjmp halt
