#include <iostream>
#include <string>
#include "eval.h"
#include "parser.h"

int main() {
    std::string line;
    Parser parser;
    while (std::getline(std::cin, line)) {
        std::cout << evaluate(parser.parse(line)) << "\n";
    }
    return 0;
}
