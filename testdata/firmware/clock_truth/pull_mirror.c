/* Internal-pull fixture for the STM32G474 (crates/hauksbee-mcu/tests/
 * renode_stm32_families.rs).
 *
 * PB11 is an input with its internal pull-up enabled and nothing else
 * configured; PA5 mirrors the level PB11 reads. On silicon an undriven PB11
 * reads high, so PA5 goes high. Renode's GPIO model ignores PUPDR, so without
 * the backend applying the pull PA5 stays low: that difference is what the
 * test measures. An external drive on PB11 must still win over the pull.
 */

#include <stdint.h>

#define REG(a) (*(volatile uint32_t *)(a))

int main(void) {
    REG(0x4002104CUL) |= 0x3UL;                               /* RCC_AHB2ENR: GPIOA, GPIOB */
    REG(0x48000000UL) = (REG(0x48000000UL) & ~(0x3UL << 10)) | (0x1UL << 10); /* PA5 output */
    REG(0x48000400UL) &= ~(0x3UL << 22);                      /* PB11 input */
    REG(0x4800040CUL) = (REG(0x4800040CUL) & ~(0x3UL << 22)) | (0x1UL << 22); /* PB11 pull-up */
    for (;;) {
        REG(0x48000014UL) = (REG(0x48000410UL) & (1UL << 11)) ? (1UL << 5) : 0UL;
    }
}
