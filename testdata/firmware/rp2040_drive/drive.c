/* RP2040 pad drive-strength fixture: set GPIO25's PADS_BANK0 DRIVE field to
 * DRIVE (0 = 2 mA, 1 = 4 mA, 2 = 8 mA, 3 = 12 mA; RP2040 datasheet Table 341,
 * bits 5:4), then drive GP25 high for good. No SDK: the vector table, the stack
 * pointer and the peripheral writes are all here, so the only thing the run
 * depends on is the register the firmware wrote.
 */
#include <stdint.h>

#ifndef DRIVE
#error "define DRIVE (0..3)"
#endif

/* The boot ROM copies the 256-byte second stage from the start of flash, checks
 * its CRC32 and jumps to it; without a valid one it falls into USB boot and never
 * reaches this code. boot2.bin is the pico-sdk W25Q080 second stage (BSD-3, Raspberry
 * Pi) taken from testdata/firmware/rp2040_blink_uart; it hands over to the vector
 * table at 0x10000100, which the linker script places right after it. */
__asm__(".section .boot2,\"a\"\n.incbin \"boot2.bin\"\n.text\n");

#define REG(a) (*(volatile uint32_t *)(a))

#define RESETS_RESET     0x4000c000UL /* 0 = every block out of reset */
#define IO_BANK0_GPIO25  0x400140ccUL /* GPIO25_CTRL: FUNCSEL bits 4:0 */
#define PADS_GPIO25      0x4001c068UL /* PADS_BANK0 GPIO25: DRIVE bits 5:4 */
#define SIO_OE_SET       0xd0000024UL
#define SIO_OUT_SET      0xd0000014UL

int main(void) {
    REG(RESETS_RESET) = 0;
    REG(PADS_GPIO25) = (REG(PADS_GPIO25) & ~(3UL << 4)) | ((uint32_t)DRIVE << 4);
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
