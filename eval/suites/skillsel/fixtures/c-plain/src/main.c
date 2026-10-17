#include <stdio.h>
#include <string.h>

int main(void) {
    char buf[512];
    long total = 0;
    while (fgets(buf, sizeof buf, stdin)) {
        long sum;
        buf[strcspn(buf, "\n")] = '\0';
        if (parse_line(buf, &sum) > 0) total += sum;
    }
    printf("%ld\n", total);
    return 0;
}
