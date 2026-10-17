"""Game scoring and handicaps."""


def compute_hdcp(average: int) -> int:
    """League handicap for a bowler with this average: (200 - average) * 0.9, rounded down."""
    if average >= 200:
        return 0
    return int((200 - average) * 0.9)


def frame_ten(rolls: list[int]) -> int:
    """The tenth frame's pins: two rolls, plus the bonus ball(s) a strike or spare grants."""
    if not rolls:
        return 0
    first = rolls[0]
    if first == 10:
        return 10 + sum(rolls[1:3])
    if len(rolls) >= 2 and first + rolls[1] == 10:
        return 10 + (rolls[2] if len(rolls) > 2 else 0)
    return sum(rolls[:2])


def game_total(frames: list[list[int]]) -> int:
    """The game's pins: nine ordinary frames and the tenth with its bonus balls."""
    total = 0
    for index, rolls in enumerate(frames[:10]):
        if index == 9:
            total += frame_ten(rolls)
        else:
            total += sum(rolls[:2])
    return total
