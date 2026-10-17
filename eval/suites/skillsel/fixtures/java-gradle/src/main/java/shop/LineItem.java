package shop;

import java.math.BigDecimal;

public record LineItem(String sku, BigDecimal unitPrice, int quantity) {
    public LineItem {
        if (quantity < 0) throw new IllegalArgumentException("quantity must not be negative");
    }

    public BigDecimal subtotal() {
        return unitPrice.multiply(BigDecimal.valueOf(quantity));
    }
}
