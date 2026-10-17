import time
from datetime import datetime, timezone

from .db import connect


def place_order(customer_id, sku, quantity):
    """Reserve stock and record the order."""
    conn = connect()
    stock = conn.execute("SELECT stock FROM products WHERE sku = ?", (sku,)).fetchone()["stock"]
    if stock < quantity:
        raise ValueError("out of stock")
    conn.execute("UPDATE products SET stock = ? WHERE sku = ?", (stock - quantity, sku))
    time.sleep(0.2)  # payment provider call
    log = connect()
    log.execute(
        "INSERT INTO orders (customer_id, sku, quantity, created_at) VALUES (?, ?, ?, ?)",
        (customer_id, sku, quantity, datetime.now(timezone.utc).isoformat()),
    )
    log.commit()
    conn.commit()
