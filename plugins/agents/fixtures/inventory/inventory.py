"""A small stock-keeping module."""


class OutOfStock(Exception):
    """Raised when more items are removed than are in stock."""


class Inventory:
    def __init__(self):
        self._stock = {}

    def add(self, item, quantity=1):
        if quantity <= 0:
            raise ValueError("quantity must be positive")
        self._stock[item] = self._stock.get(item, 0) + quantity

    def remove(self, item, quantity=1):
        available = self._stock.get(item, 0)
        if quantity > available:
            raise OutOfStock(f"only {available} {item} left")
        self._stock[item] = available - quantity
        if self._stock[item] == 0:
            del self._stock[item]

    def count(self, item):
        return self._stock.get(item, 0)

    def total_items(self):
        return len(self._stock)

    def report(self):
        return ", ".join(f"{item}={count}" for item, count in sorted(self._stock.items()))
