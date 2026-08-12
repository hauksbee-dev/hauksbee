; ATmega328P proof for atomic AT28 control updates.
; One OUT changes PD2=/WE and PD3=/OE together for the first byte after a read.

	.section .text
	.global _start
_start:
	cli
	ldi r16, 0x08
	sts 0x5e, r16
	ldi r16, 0xff
	sts 0x5d, r16

	; PASS (PC1), address A0 (PC0), and data D6/D7 (PC4/PC5).
	ldi r16, 0x33
	out 0x07, r16
	ldi r16, 0x3f
	out 0x04, r16          ; PB0..5 are D0..5
	ldi r16, 0x0c
	out 0x0a, r16          ; PD2=/WE, PD3=/OE

	; Begin in read state, then inhibit output while preparing the bus.
	ldi r16, 0x04
	out 0x0b, r16          ; /WE high, /OE low
	ldi r16, 0x00
	out 0x0b, r16          ; /WE low, /OE low

	; Address 0, data 0x91.
	ldi r16, 0x11
	out 0x05, r16
	ldi r16, 0x20
	out 0x08, r16

	; Historical boundary: one hardware port write raises /WE and /OE.
	ldi r16, 0x0c
	out 0x0b, r16

	; Address 1, data 0xe2, then an ordinary /WE pulse.
	ldi r16, 0x22
	out 0x05, r16
	ldi r16, 0x31
	out 0x08, r16
	ldi r16, 0x08
	out 0x0b, r16
	ldi r16, 0x0c
	out 0x0b, r16

	; Release data bus, assert /OE to close/program the page.
	ldi r16, 0x00
	out 0x04, r16
	ldi r16, 0x03
	out 0x07, r16
	ldi r16, 0x04
	out 0x0b, r16

	; >10 ms at 16 MHz.
	ldi r20, 4
wait_outer:
	ldi r19, 255
wait_mid:
	ldi r18, 255
wait_inner:
	dec r18
	brne wait_inner
	dec r19
	brne wait_mid
	dec r20
	brne wait_outer

	; Refresh settled output, then verify address 0 = 0x91.
	ldi r16, 0x0c
	out 0x0b, r16
	ldi r16, 0x02
	out 0x08, r16          ; A0 low, PASS low
	ldi r16, 0x04
	out 0x0b, r16
	in r17, 0x03
	andi r17, 0x3f
	cpi r17, 0x11
	brne fail
	in r17, 0x06
	andi r17, 0x30
	cpi r17, 0x20
	brne fail

	; Address 1 = 0xe2. Address edge refreshes responder output.
	ldi r16, 0x03
	out 0x08, r16
	in r17, 0x03
	andi r17, 0x3f
	cpi r17, 0x22
	brne fail
	in r17, 0x06
	andi r17, 0x30
	cpi r17, 0x30
	brne fail

	; PASS high only after both bytes read back from programmed storage.
	ldi r16, 0x03
	out 0x08, r16
pass:
	rjmp pass
fail:
	ldi r16, 0x01
	out 0x08, r16          ; A0 high, PASS remains low
	rjmp fail
