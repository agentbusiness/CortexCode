package shop;

import static org.junit.jupiter.api.Assertions.assertEquals;

import java.math.BigDecimal;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import org.junit.jupiter.api.Test;

class OrderServiceTest {
    private final OrderService service = new OrderService(Map.of("TEN", 10));

    @Test
    void totalAppliesCoupon() {
        Order order = new Order("o1", List.of(new LineItem("a", new BigDecimal("10.00"), 2)), "TEN");
        assertEquals(new BigDecimal("18.00"), service.total(order));
    }

    @Test
    void dropsEmptyLines() {
        List<LineItem> items = new ArrayList<>(List.of(
            new LineItem("a", BigDecimal.ONE, 0),
            new LineItem("b", BigDecimal.ONE, 0),
            new LineItem("c", BigDecimal.ONE, 1)));
        service.dropEmptyLines(items);
        assertEquals(1, items.size());
    }
}
