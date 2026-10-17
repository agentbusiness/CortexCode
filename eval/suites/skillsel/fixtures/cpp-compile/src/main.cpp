#include <iostream>
#include <string>
#include "eval.h"
#include "parser.h"

int main() {
    std::string line;
    Parser parser;
    const int precision = 2;
    while (std::getline(std::cin, line)) {
        std::cout << evaluate(parser.parse(line), precision) << "\n";
    }
    return 0;
}
