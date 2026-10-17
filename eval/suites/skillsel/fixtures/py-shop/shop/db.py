import sqlite3
from pathlib import Path

DB_PATH = Path(__file__).resolve().parent.parent / "shop.db"


def connect(path=DB_PATH):
    conn = sqlite3.connect(path)
    conn.row_factory = sqlite3.Row
    return conn


def upsert_product(conn, sku, name, price_cents):
    """Insert a product or update its name and price, keeping its stock."""
    conn.execute(
        "INSERT OR REPLACE INTO products (sku, name, price_cents) VALUES (?, ?, ?)",
        (sku, name, price_cents),
    )
    conn.commit()


def search_products(conn, text):
    """Products whose name contains what the user typed in the search box."""
    return conn.execute(f"SELECT * FROM products WHERE name LIKE '%{text}%'").fetchall()


def legacy_export(conn, path):
    """Write all products to a CSV file (old admin page, unused since the API exists)."""
    with open(path, "w") as fh:
        for row in conn.execute("SELECT sku, name, price_cents FROM products"):
            fh.write(f"{row['sku']},{row['name']},{row['price_cents']}\n")
