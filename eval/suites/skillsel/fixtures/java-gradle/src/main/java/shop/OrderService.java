package shop;

import java.math.BigDecimal;
import java.math.RoundingMode;
import java.util.Map;

public class OrderService {
    private final Map<String, Integer> couponPercent;

    public OrderService(Map<String, Integer> couponPercent) {
        this.couponPercent = couponPercent;
    }

    /** Sum of the line subtotals, minus the coupon's percentage if the order has a known coupon, in cents precision. */
    public BigDecimal total(Order order) {
        BigDecimal sum = order.items().stream().map(LineItem::subtotal).reduce(BigDecimal.ZERO, BigDecimal::add);
        Integer percent = order.couponCode() == null ? null : couponPercent.get(order.couponCode());
        if (percent != null) {
            sum = sum.multiply(BigDecimal.valueOf(100 - percent)).divide(BigDecimal.valueOf(100));
        }
        return sum.setScale(2, RoundingMode.HALF_EVEN);
    }
}
