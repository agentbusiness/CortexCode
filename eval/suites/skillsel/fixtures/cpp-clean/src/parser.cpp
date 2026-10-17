#include "parser.h"
#include <cctype>
#include <cstdlib>

std::vector<Token> Parser::parse(const std::string& text) const {
    std::vector<Token> out;
    for (size_t i = 0; i < text.size();) {
        char c = text[i];
        if (std::isspace(static_cast<unsigned char>(c))) { ++i; continue; }
        if (c == '+') { out.push_back({Token::Plus}); ++i; continue; }
        if (c == '-') { out.push_back({Token::Minus}); ++i; continue; }
        char* end = nullptr;
        double v = std::strtod(text.c_str() + i, &end);
        out.push_back({Token::Number, v});
        i = static_cast<size_t>(end - text.c_str());
    }
    return out;
}
