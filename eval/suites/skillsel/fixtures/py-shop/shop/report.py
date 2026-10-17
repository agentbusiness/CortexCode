from .db import connect


def orders_per_customer(conn=None):
    """(email, number of orders) for every customer, including customers with no orders."""
    conn = conn or connect()
    return conn.execute(
        """
        SELECT c.email, COUNT(*) AS orders
        FROM customers c LEFT JOIN orders o ON o.customer_id = c.id
        GROUP BY c.email
        ORDER BY c.email
        """
    ).fetchall()


def daily_sales(day, conn=None):
    """Revenue in cents per product on one day (YYYY-MM-DD)."""
    conn = conn or connect()
    return conn.execute(
        """
        SELECT o.sku,
               (SELECT p.price_cents FROM products p WHERE p.sku = o.sku) * SUM(o.quantity) AS cents
        FROM orders o
        WHERE date(o.created_at) = ?
        GROUP BY o.sku
        """,
        (day,),
    ).fetchall()
