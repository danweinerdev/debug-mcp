CC ?= gcc
CFLAGS = -g -O0 -fno-omit-frame-pointer

SOURCES = simple.c loop.c crash.c structs.c multithread.c
BINARIES = $(SOURCES:.c=)

all: $(BINARIES)

simple: simple.c
	$(CC) $(CFLAGS) -o $@ $<

loop: loop.c
	$(CC) $(CFLAGS) -o $@ $<

crash: crash.c
	$(CC) $(CFLAGS) -o $@ $<

structs: structs.c
	$(CC) $(CFLAGS) -o $@ $<

multithread: multithread.c
	$(CC) $(CFLAGS) -pthread -o $@ $<

clean:
	rm -f $(BINARIES)

.PHONY: all clean
