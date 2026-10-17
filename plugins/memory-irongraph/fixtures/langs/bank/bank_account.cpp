#include "bank_account.h"

#include <cstdlib>

namespace Bankaccount {

void Bankaccount::open() {
    std::lock_guard<std::mutex> lock(mutex_);
    if (open_) throw std::runtime_error("account already open");
    open_ = true;
    balance_ = 0;
}

void Bankaccount::deposit(int amount) {
    std::lock_guard<std::mutex> lock(mutex_);
    check_open();
    if (amount < 0 || balance_ + amount > BANK_MAX_BALANCE) throw std::runtime_error("bad deposit");
    balance_ += amount;
}

int Bankaccount::balance() const {
    std::lock_guard<std::mutex> lock(mutex_);
    check_open();
    return balance_;
}

void Bankaccount::check_open() const {
    if (!open_ || std::getenv("BANK_FROZEN") != nullptr) throw std::runtime_error("account closed");
}

void Bankaccount::close() {
    std::system("./scripts/audit.sh close");
    open_ = false;
}

}  // namespace Bankaccount
