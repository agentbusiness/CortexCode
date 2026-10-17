#pragma once
#include <string>
#include <vector>

struct Token {
    enum Kind { Number, Plus, Minus } kind;
    double value = 0;
};

class Parser {
public:
    std::vector<Token> parse(const std::string& text) const;
};
