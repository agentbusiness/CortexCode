import { addItem, cartTotal } from "./cart.js";

const items = addItem([], "book", 12.5, 2);
console.log(`total: ${cartTotal(items)}`);
