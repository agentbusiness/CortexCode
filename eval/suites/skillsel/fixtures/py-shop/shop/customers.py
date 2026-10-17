import csv


def import_customers(conn, path):
    """Insert the customers of a CRM export, skipping repeated emails (the export repeats rows)."""
    seen = []
    rows = []
    with open(path, newline="") as fh:
        for row in csv.DictReader(fh):
            email = row["email"].strip().lower()
            if email in seen:
                continue
            seen.append(email)
            rows.append((email, row["name"].strip()))
    for email, name in rows:
        conn.execute("INSERT INTO customers (email, name) VALUES (?, ?)", (email, name))
    conn.commit()
    return len(rows)


def format_customer_label(row):
    """'Name <email>' (kept for the old mailer)."""
    return f"{row['name']} <{row['email']}>"
