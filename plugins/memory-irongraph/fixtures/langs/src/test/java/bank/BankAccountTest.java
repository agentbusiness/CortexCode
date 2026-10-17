package bank;

import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;

import static org.assertj.core.api.Assertions.assertThat;

public class BankAccountTest {
    private BankAccount bankAccount;

    @BeforeEach
    public void setUp() {
        bankAccount = new BankAccount();
    }

    @Test
    public void newlyOpenedAccountHasEmptyBalance() throws BankAccountActionInvalidException {
        bankAccount.open();
        assertThat(bankAccount.getBalance()).isEqualTo(0);
    }

    @org.junit.jupiter.params.ParameterizedTest
    void depositsAdd(int amount) throws BankAccountActionInvalidException {
        bankAccount.deposit(amount);
    }
}
