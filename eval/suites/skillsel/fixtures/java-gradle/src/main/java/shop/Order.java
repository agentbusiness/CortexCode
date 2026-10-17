package shop;

import java.util.List;

public record Order(String id, List<LineItem> items, String couponCode) {}
