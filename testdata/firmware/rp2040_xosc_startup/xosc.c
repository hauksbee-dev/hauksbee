/* RP2040 crystal start-up fixture: program XOSC_STARTUP.DELAY to DELAY counts,
 * enable the crystal oscillator, wait for XOSC_STATUS.STABLE, switch clk_ref
 * onto the crystal, then drive GP25 high for good.
 *
 * STARTUP.DELAY is "how many clock cycles must be seen from the crystal before
 * it can be used ... specified in multiples of 256" (RP2040 datasheet s2.16.3),
 * so STABLE is a counter, not a measurement: it rises after DELAY x 256 cycles
 * whether or not the oscillation is large enough yet. At 12 MHz one count is
 * 21.33 us; DELAY = 47 is the pico-sdk's ~1 ms, DELAY = 3000 is 64 ms. A
 * crystal that needs longer than the programmed delay hands the core a clock
 * that is not there yet. No SDK: the vector table, the stack pointer and the
 * register writes are all here.
 */
#include <stdint.h>

#ifndef DELAY
#error "define DELAY (XOSC_STARTUP.DELAY counts of 256 crystal cycles)"
#endif

/* The boot ROM copies the 256-byte second stage from the start of flash, checks
 * its CRC32 and jumps to it; without a valid one it falls into USB boot. boot2.bin
 * is the pico-sdk W25Q080 second stage (BSD-3, Raspberry Pi) taken from
 * testdata/firmware/rp2040_blink_uart; it hands over to the vector table at
 * 0x10000100, which the linker script places right after it. */
__asm__(".section .boot2,\"a\"\n.incbin \"boot2.bin\"\n.text\n");

#define REG(a) (*(volatile uint32_t *)(a))

#define RESETS_RESET      0x4000c000UL /* 0 = every block out of reset */
#define XOSC_CTRL         0x40024000UL /* FREQ_RANGE 11:0, ENABLE 23:12 */
#define XOSC_STATUS       0x40024004UL /* STABLE bit 31 */
#define XOSC_STARTUP      0x4002400cUL /* DELAY 13:0 */
#define CLK_REF_CTRL      0x40008030UL /* SRC 1:0, 2 = xosc_clksrc */
#define IO_BANK0_GPIO25   0x400140ccUL /* GPIO25_CTRL: FUNCSEL bits 4:0 */
#define SIO_OE_SET        0xd0000024UL
#define SIO_OUT_SET       0xd0000014UL

int main(void) {
    REG(RESETS_RESET) = 0;
    REG(XOSC_STARTUP) = DELAY;
    REG(XOSC_CTRL) = (0xFABUL << 12) | 0xAA0UL; /* enable, 1-15 MHz range */
    while (!(REG(XOSC_STATUS) & (1UL << 31))) {
    }
    REG(CLK_REF_CTRL) = 2; /* clk_ref from the crystal */
    REG(IO_BANK0_GPIO25) = 5; /* FUNCSEL = SIO */
    REG(SIO_OE_SET) = 1UL << 25;
    REG(SIO_OUT_SET) = 1UL << 25;
    for (;;) {
    }
}

__attribute__((naked, noreturn)) void Reset_Handler(void) {
    __asm__ volatile("ldr r0, =0x20042000\n"
                     "mov sp, r0\n"
                     "bl main\n"
                     "1: b 1b\n");
}

__attribute__((section(".isr_vector"), used)) void (*const vector_table[])(void) = {
    (void (*)(void))0x20042000UL,
    Reset_Handler,
};
