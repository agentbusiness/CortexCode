#include <cassert>
#include <cstdio>
#include "eval.h"
#include "parser.h"

int main() {
    Parser parser;
    assert(evaluate(parser.parse("1 + 2")) == 3);
    assert(evaluate(parser.parse("10 - 4 + 1")) == 7);
    assert(evaluate(parser.parse("")) == 0);
    std::puts("eval tests passed");
    return 0;
}
