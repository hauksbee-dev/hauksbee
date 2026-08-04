// Negative control for the two-sided proof: the same pico-sdk runtime and the
// same UART chatter, but main() never touches a GPIO. A toggle assertion on
// GP25 must FAIL against this image.
#include <stdio.h>
#include "pico/stdlib.h"

int main(void) {
    stdio_init_all();
    printf("hauksbee rp2040: main reached\n");
    for (int i = 0; i < 200; i++) {
        printf("quiet %d\n", i);
        sleep_ms(20);
    }
    printf("hauksbee rp2040: done\n");
    while (1) {
        tight_loop_contents();
    }
}
