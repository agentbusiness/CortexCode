CC ?= cc
CFLAGS ?= -std=c11 -Wall -Werror -O2

csvsum: src/main.c src/parse.c
	$(CC) $(CFLAGS) -o $@ src/main.c src/parse.c

clean:
	rm -f csvsum
