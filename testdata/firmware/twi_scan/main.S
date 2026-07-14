; TWI (I2C) bus scanner for the hauksbee AVR co-sim ACK-gating proof.
;
; Scans every 7-bit address in 0x08..0x77 over the ATmega328P hardware TWI
; peripheral: START, SLA+W, check the TWSR status, STOP. Each address that
; ACKed (TWSR status 0x18, TW_MT_SLA_ACK) is emitted as one raw byte on
; UART0; after the sweep a single 0xFE terminator byte is sent and the
; firmware parks in an infinite loop.
;
; This is the classic "i2c_scanner" shape: before the ACK gate, the simavr
; bridge ACKed every address unconditionally, so this scan "found" a device
; at all 112 addresses. With the gate, it must find exactly the modeled
; slaves and see honest NACKs everywhere else.
;
; Written in raw assembly (avr-as, no avr-libc) so the fixture rebuilds with
; just GNU binutils-avr; the committed .hex is what the tests load.
;
; ATmega328P @ 16 MHz, TWI at 100 kHz, UART0 at 9600 8N1.

	.section .text
	.global _start
_start:
	; Stack pointer -> RAMEND (0x08FF). No calls are made, but keep the
	; core in a sane state.
	ldi r16, 0x08
	sts 0x5E, r16           ; SPH
	ldi r16, 0xFF
	sts 0x5D, r16           ; SPL

	; UART0: 9600 baud @ 16 MHz -> UBRR = 103, TX only, 8N1.
	ldi r16, 103
	sts 0xC4, r16           ; UBRR0L
	ldi r16, 0
	sts 0xC5, r16           ; UBRR0H
	ldi r16, 0x08           ; TXEN0
	sts 0xC1, r16           ; UCSR0B
	ldi r16, 0x06           ; UCSZ01|UCSZ00 (8N1)
	sts 0xC2, r16           ; UCSR0C

	; TWI: SCL = F_CPU / (16 + 2*TWBR) = 100 kHz -> TWBR = 72.
	ldi r16, 72
	sts 0xB8, r16           ; TWBR
	ldi r16, 0
	sts 0xB9, r16           ; TWSR (prescaler = 1)
	ldi r16, 0x04           ; TWEN
	sts 0xBC, r16           ; TWCR

	ldi r17, 0x08           ; first scanned address
scan_loop:
	; START: TWCR = TWINT|TWSTA|TWEN
	ldi r16, 0xA4
	sts 0xBC, r16
wait_start:
	lds r16, 0xBC
	sbrs r16, 7             ; TWINT set?
	rjmp wait_start

	; SLA+W
	mov r16, r17
	lsl r16                 ; (addr << 1) | 0
	sts 0xBB, r16           ; TWDR
	ldi r16, 0x84           ; TWINT|TWEN
	sts 0xBC, r16
wait_sla:
	lds r16, 0xBC
	sbrs r16, 7
	rjmp wait_sla

	; TWSR status: 0x18 = SLA+W transmitted, ACK received.
	lds r16, 0xB9
	andi r16, 0xF8
	cpi r16, 0x18
	brne no_ack

	; Report the ACKed address on UART0.
tx_wait:
	lds r16, 0xC0           ; UCSR0A
	sbrs r16, 5             ; UDRE0
	rjmp tx_wait
	sts 0xC6, r17           ; UDR0 = address

no_ack:
	; STOP: TWCR = TWINT|TWSTO|TWEN, then a short settle delay.
	ldi r16, 0x94
	sts 0xBC, r16
	ldi r18, 200
settle:
	dec r18
	brne settle

	inc r17
	cpi r17, 0x78           ; one past the last scanned address
	brne scan_loop

	; Terminator so the harness knows the sweep completed.
tx_wait_end:
	lds r16, 0xC0
	sbrs r16, 5
	rjmp tx_wait_end
	ldi r16, 0xFE
	sts 0xC6, r16
halt:
	rjmp halt
