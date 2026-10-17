#pragma once
#include <mutex>
#include <stdexcept>

#define BANK_MAX_BALANCE 1000000
#define BANK_CHECK(x) do { if (!(x)) throw std::runtime_error(#x); } while (0)

namespace Bankaccount {

/// A bank account guarded by a mutex.
class Bankaccount {
public:
    void open();
    void close();
    void deposit(int amount);
    int balance() const;

private:
    void check_open() const;
    bool open_ = false;
    int balance_ = 0;
    mutable std::mutex mutex_;
};

enum class Kind { Checking, Savings };

}  // namespace Bankaccount
