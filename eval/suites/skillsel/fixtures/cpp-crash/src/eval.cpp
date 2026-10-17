#include "eval.h"

double evaluate(const std::vector<Token>& tokens) {
    // The first token is always a number: start from it.
    double acc = tokens.front().value;
    int sign = 1;
    for (size_t i = 1; i < tokens.size(); ++i) {
        const Token& t = tokens[i];
        if (t.kind == Token::Plus) sign = 1;
        else if (t.kind == Token::Minus) sign = -1;
        else acc += sign * t.value;
    }
    return acc;
}
