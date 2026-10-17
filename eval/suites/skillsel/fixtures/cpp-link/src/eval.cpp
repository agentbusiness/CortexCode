#include "eval.h"

double evaluate(const std::vector<Token>& tokens) {
    double acc = 0;
    int sign = 1;
    for (const auto& t : tokens) {
        if (t.kind == Token::Plus) sign = 1;
        else if (t.kind == Token::Minus) sign = -1;
        else acc += sign * t.value;
    }
    return acc;
}
