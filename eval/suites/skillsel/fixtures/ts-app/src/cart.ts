export interface Item {
  sku: string;
  price: number;
  quantity: number;
}

export function lineTotal(price: number, quantity: number): number {
  return Math.round(price * quantity * 100) / 100;
}

export function cartTotal(items: Item[]): number {
  return items.reduce((sum, item) => sum + lineTotal(item.price, item.quantity), 0);
}

export function addItem(items: Item[], sku: string, price: number, quantity = 1): Item[] {
  return [...items, { sku, price, quantity }];
}

export function applyDiscount(total: number, percent: number): number {
  return lineTotal(total, (100 - percent) / 100);
}

export function totalWithShipping(items: Item[], shipping: string): number {
  return lineTotal(cartTotal(items) + shipping, 1);
}
