import static org.assertj.core.api.Assertions.assertThat;
import static org.assertj.core.api.Assertions.assertThatThrownBy;

import org.junit.jupiter.api.Disabled;
import org.junit.jupiter.api.Test;

public class HammingTest {
    @Test
    public void testNoDistanceBetweenEmptyStrands() {
        assertThat(new Hamming("", "").getHammingDistance()).isEqualTo(0);
    }

    @Disabled("Remove to run test")
    @Test
    public void testDistanceInLongDifferentStrands() {
        assertThat(new Hamming("GGACGGATTCTG", "AGGACGGATTCT").getHammingDistance()).isEqualTo(9);
    }

    @Disabled("Remove to run test")
    @Test
    public void testValidatesFirstStrandNotLonger() {
        assertThatThrownBy(() -> new Hamming("AATG", "AAA"))
            .isInstanceOf(IllegalArgumentException.class)
            .hasMessage("strands must be of equal length");
    }
}
