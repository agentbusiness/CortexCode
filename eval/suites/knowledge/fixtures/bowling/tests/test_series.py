from bowling.series import series_total


def test_series_adds_the_handicap_per_game():
    plain = [[[9, 0]] * 10] * 3
    assert series_total(plain, 150) == 3 * 90 + 3 * 45
