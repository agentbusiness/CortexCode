import csv
from pathlib import Path

from .dates import parse_date
from .retry import retry


def read_rows(path: Path):
    """Rows of the bookings export with parsed dates."""
    def open_file():
        return path.open(newline="")

    with retry(open_file) as fh:
        for row in csv.DictReader(fh):
            yield {"id": int(row["id"]), "guest": row["guest"], "arrival": parse_date(row["arrival"])}
