package shop;

import java.math.BigDecimal;

/** Plain-text receipt of an order, one line per item. */
public final class Receipt {
    private Receipt() {}

    public static String render(Order order, BigDecimal total) {
        StringBuilder out = new StringBuilder("Order " + order.id() + "\n");
        for (LineItem item : order.items()) {
            out.append(item.quantity()).append(" x ").append(item.sku())
               .append(" @ ").append(item.price()).append("\n");
        }
        return out.append("Total: ").append(total).append("\n").toString();
    }
}
