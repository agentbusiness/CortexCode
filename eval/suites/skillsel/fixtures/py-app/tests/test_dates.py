from datetime import date

from importer.dates import parse_date


def test_day_first():
    assert parse_date("03/04/2026") == date(2026, 4, 3)


def test_late_day():
    assert parse_date("25/12/2026") == date(2026, 12, 25)
