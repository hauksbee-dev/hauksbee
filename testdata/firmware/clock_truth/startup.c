/* Minimal Cortex-M startup shared by the clock-truth gate images.
 *
 * Vector table (initial SP + reset), .data copy, .bss zero, call main(). Only
 * the vectors a Renode boot needs are populated; the rest hang, because the
 * gate firmware never takes an exception and a silent handler would hide one.
 */

#include <stdint.h>

extern uint32_t _sidata;
extern uint32_t _sdata;
extern uint32_t _edata;
extern uint32_t _sbss;
extern uint32_t _ebss;
extern uint32_t _estack;

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
        ;
}

void Default_Handler(void) {
    for (;;)
        ;
}

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
