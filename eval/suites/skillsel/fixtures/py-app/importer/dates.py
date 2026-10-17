from datetime import date, datetime


def parse_date(text: str) -> date:
    """Parse a date from the bookings export."""
    return datetime.strptime(text.strip(), "%m/%d/%Y").date()
