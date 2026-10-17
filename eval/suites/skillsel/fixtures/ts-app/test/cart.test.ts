import { describe, expect, it } from "vitest";
import { cartTotal, lineTotal } from "../src/cart";

describe("cart", () => {
  it("rounds line totals to cents", () => {
    expect(lineTotal(0.1, 3)).toBe(0.3);
  });
  it("sums lines", () => {
    expect(cartTotal([{ sku: "a", price: 2, quantity: 2 }, { sku: "b", price: 1.5, quantity: 1 }])).toBe(5.5);
  });
});
