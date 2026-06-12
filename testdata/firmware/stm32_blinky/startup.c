/* Minimal Cortex-M3 startup for the galvani STM32F103 demo firmware.
 *
 * Provides the initial vector table (stack pointer + reset handler), copies
 * .data from flash to RAM, zeroes .bss, then calls main(). Only the two
 * vectors Renode needs to boot are populated; the rest default to a hang.
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
    /* Copy initialised data from flash to RAM. */
    uint32_t *src = &_sidata;
    uint32_t *dst = &_sdata;
    while (dst < &_edata)
        *dst++ = *src++;

    /* Zero the .bss section. */
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

/* Vector table placed at the start of flash by the linker script. */
__attribute__((section(".isr_vector"), used))
void (*const vector_table[])(void) = {
    (void (*)(void))&_estack, /* 0x00: initial stack pointer */
    Reset_Handler,            /* 0x04: reset */
    Default_Handler,          /* 0x08: NMI */
    Default_Handler,          /* 0x0C: HardFault */
    Default_Handler,          /* 0x10: MemManage */
    Default_Handler,          /* 0x14: BusFault */
    Default_Handler,          /* 0x18: UsageFault */
};
