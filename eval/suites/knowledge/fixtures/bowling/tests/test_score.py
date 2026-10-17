from bowling.score import compute_hdcp, frame_ten, game_total


def test_handicap_usbc():
    assert compute_hdcp(150) == 45
    assert compute_hdcp(200) == 0


def test_frame_ten_strike_gets_two_bonus_balls():
    assert frame_ten([10, 10, 10]) == 30
    assert frame_ten([7, 3, 5]) == 15
    assert frame_ten([7, 2]) == 9


def test_game_total_counts_the_tenth_frame_bonus():
    frames = [[9, 0]] * 9 + [[10, 10, 10]]
    assert game_total(frames) == 81 + 30
