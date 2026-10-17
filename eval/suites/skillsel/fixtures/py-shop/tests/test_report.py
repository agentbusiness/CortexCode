import sqlite3
from pathlib import Path

from shop.report import orders_per_customer

SCHEMA = (Path(__file__).resolve().parent.parent / "schema.sql").read_text()


def test_orders_per_customer_counts_orders():
    conn = sqlite3.connect(":memory:")
    conn.row_factory = sqlite3.Row
    conn.executescript(SCHEMA)
    conn.execute("INSERT INTO customers (id, email, name) VALUES (1, 'a@x', 'A'), (2, 'b@x', 'B')")
    conn.execute("INSERT INTO products (sku, name, price_cents) VALUES ('s', 'Soap', 300)")
    conn.execute("INSERT INTO orders (customer_id, sku, quantity, created_at) VALUES (1, 's', 1, '2026-10-01T10:00:00')")
    rows = [tuple(r) for r in orders_per_customer(conn)]
    assert rows[0] == ("a@x", 1)
