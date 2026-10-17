"""Series: three games and the handicap."""

from bowling.score import compute_hdcp


def series_total(games: list[list[list[int]]], average: int) -> int:
    """Three games plus the handicap per game.

    Each game's frames are summed as two rolls each; the tenth frame's bonus balls are part of the
    game total (see score.game_total).
    """
    total = 0
    for frames in games:
        for rolls in frames[:10]:
            total += sum(rolls[:2])
    return total + 3 * compute_hdcp(average)
