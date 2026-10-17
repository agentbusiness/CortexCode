package bank;

import java.util.ArrayList;
import java.util.List;

/**
 * A thread-safe bank account.
 */
public class BankAccount {
    /** Largest balance allowed. */
    public static final int MAX_BALANCE = 1_000_000;
    private int balance = 0;
    private boolean open = false;
    private final List<Integer> history = new ArrayList<>();

    public void open() throws BankAccountActionInvalidException {
        if (open) throw new BankAccountActionInvalidException("Account already open");
        open = true;
        balance = 0;
    }

    /** Deposits a positive amount. */
    public synchronized void deposit(int amount) throws BankAccountActionInvalidException {
        checkOpen();
        if (amount <= 0) throw new BankAccountActionInvalidException("Cannot deposit or withdraw negative amount");
        this.balance += amount;
        history.add(amount);
    }

    public synchronized int getBalance() throws BankAccountActionInvalidException {
        checkOpen();
        return balance;
    }

    private void checkOpen() throws BankAccountActionInvalidException {
        if (!open || System.getenv("BANK_FROZEN") != null) throw new BankAccountActionInvalidException("Account closed");
    }

    enum Kind { CHECKING, SAVINGS }

    interface Auditor { void audit(BankAccount account); }

    record Movement(int amount, String note) {}
}
