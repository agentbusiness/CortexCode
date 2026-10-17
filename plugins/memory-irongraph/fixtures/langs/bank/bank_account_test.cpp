#include "bank_account.h"
#include <catch2/catch_test_macros.hpp>
#include <algorithm>
#include <vector>

TEST_CASE("newly opened account has zero balance", "[bank]") {
    Bankaccount::Bankaccount account{};
    account.open();
    REQUIRE(account.balance() == 0);
}

TEST(BankaccountTest, Deposits) {
    auto account = std::make_unique<Bankaccount::Bankaccount>();
    account->open();
    account->deposit(5);
}

// Negative control: an ordinary function whose name looks like a test macro.
int TESTS(int n) {
    std::vector<int> v{3, 1, 2};
    std::sort(v.begin(), v.end());
    return n;
}
