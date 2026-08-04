/* Minimal Cortex-M0 startup for the hauksbee STM32F072 walkthrough firmware.
 *
 * Provides the initial vector table (stack pointer + reset handler), copies
 * .data from flash to RAM, zeroes .bss, then calls main(). Only the vectors
 * Renode needs to boot are populated; the rest default to a hang.
 */

#include <stdint.h>

extern uint32_t _sidata; /* start of .data init values in flash */
extern uint32_t _sdata;  /* start of .data in RAM */
extern uint32_t _edata;  /* end of .data in RAM */
extern uint32_t _sbss;   /* start of .bss */
extern uint32_t _ebss;   /* end of .bss */
extern uint32_t _estack; /* top of stack (from linker) */

int main(void);

void Reset_Handler(void) {
    uint32_t *src = &_sidata;
    uint32_t *dst = &_sdata;
    while (dst < &_edata)
        *dst++ = *src++;

    dst = &_sbss;
    while (dst < &_ebss)
        *dst++ = 0;

    main();

    for (;;)
        ; /* main never returns */
}

void Default_Handler(void) {
    for (;;)
        ;
}

/* Vector table placed at the start of flash by the linker script. The Cortex-M0
 * has no MemManage/BusFault/UsageFault vectors, so the table is shorter than
 * the M3's. */
__attribute__((section(".isr_vector"), used))
void (*const vector_table[])(void) = {
    (void (*)(void))&_estack, /* 0x00: initial stack pointer */
    Reset_Handler,            /* 0x04: reset */
    Default_Handler,          /* 0x08: NMI */
    Default_Handler,          /* 0x0C: HardFault */
};
