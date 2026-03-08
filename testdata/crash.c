#include <stdio.h>
#include <stdlib.h>

int main(void) {
    printf("about to crash\n");
    int *p = NULL;
    *p = 42;  // NULL pointer dereference
    return 0;
}
