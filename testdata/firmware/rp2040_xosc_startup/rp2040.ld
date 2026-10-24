/* RP2040 XIP flash at 0x10000000, 264 KiB SRAM at 0x20000000. */
ENTRY(Reset_Handler)
MEMORY
{
    FLASH (rx)  : ORIGIN = 0x10000000, LENGTH = 2M
    RAM   (rwx) : ORIGIN = 0x20000000, LENGTH = 264K
}
SECTIONS
{
    .boot2 : { KEEP(*(.boot2)) } > FLASH
    .isr_vector : { KEEP(*(.isr_vector)) } > FLASH
    .text : { *(.text*) *(.rodata*) } > FLASH
}
