/* FE310 clock-truth oracle: everything (code, data, stack) in the 16 KB DTIM
 * at 0x8000_0000. No flash/XIP link: `sysbus LoadELF` writes the sections
 * straight into the mapped RAM and `post_load_setup` points the PC at `vinit`,
 * so an XIP layout would only add a copy loop with nothing to copy it. */
ENTRY(vinit)

MEMORY
{
  DTIM (rwx) : ORIGIN = 0x80000000, LENGTH = 16K
}

SECTIONS
{
  .text : {
    *(.text.vinit)
    *(.text*)
  } > DTIM

  .rodata : { *(.rodata*) *(.srodata*) } > DTIM
  .data   : { *(.data*) *(.sdata*) } > DTIM
  .bss    : { *(.bss*) *(.sbss*) } > DTIM

  _stack_top = ORIGIN(DTIM) + LENGTH(DTIM);
}
