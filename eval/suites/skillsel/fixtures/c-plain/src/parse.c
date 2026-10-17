#include <stdlib.h>
#include "parse.h"

int parse_line(const char *line, long *sum) {
    int fields = 0;
    char *end;
    *sum = 0;
    while (*line) {
        long v = strtol(line, &end, 10);
        if (end == line) break;
        *sum += v;
        fields++;
        line = (*end == ',') ? end + 1 : end;
    }
    return fields;
}
