/* Windows debugging fixture for the WinDbg (DbgEng) backend live tests.
 *
 * Ported from the C++ plugin's test/test_target.cpp scenarios. One small program with several
 * behaviors selected by argv[1]; built Debug (/Zi /Od) so a PDB exists and locals are clean.
 *
 *   (none) | normal  : run `compute(10)`, print the result, exit 0   (launch / step / locals)
 *   null             : null-pointer write  -> access violation        (crash analysis)
 *   av               : wild-pointer write  -> access violation        (crash analysis)
 *   wait             : sleep forever                                   (attach-by-pid testing)
 *
 * Keep this dependency-free (only the CRT + Win32 Sleep) so `cl /Zi /Od` builds it with no
 * extra libs. The named functions (`compute`, `crash_null`, `crash_av`, `wait_forever`, `main`)
 * are the breakpoint/stack targets the tests resolve by name.
 */
#include <stdio.h>
#include <string.h>
#include <windows.h>

/* A function with locals (`sum`, `i`) to inspect at a breakpoint, and a stable return value. */
static int compute(int n) {
    int sum = 0;
    for (int i = 0; i < n; i++) {
        sum += i;
    }
    return sum; /* compute(10) == 45 */
}

/* Null-pointer write -> EXCEPTION_ACCESS_VIOLATION (write to 0x0). */
static void crash_null(void) {
    volatile int *p = NULL;
    *p = 42;
}

/* Wild-pointer write -> EXCEPTION_ACCESS_VIOLATION at a recognizable address. */
static void crash_av(void) {
    volatile char *p = (volatile char *)(size_t)0xDEADBEEF;
    *p = 1;
}

/* Spin forever so a test can attach by pid and then break in. */
static void wait_forever(void) {
    for (;;) {
        Sleep(1000);
    }
}

int main(int argc, char **argv) {
    const char *mode = (argc > 1) ? argv[1] : "normal";

    if (strcmp(mode, "null") == 0) {
        crash_null();
    } else if (strcmp(mode, "av") == 0) {
        crash_av();
    } else if (strcmp(mode, "wait") == 0) {
        wait_forever();
    } else {
        int r = compute(10);
        printf("compute(10) = %d\n", r);
    }
    return 0;
}
