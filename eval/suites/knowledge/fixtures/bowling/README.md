# Bowling

A league scoring app: games, series and handicaps for ten-pin bowling leagues.

## Scoring

A game has ten frames of two rolls each. A strike (all ten pins with the first ball) scores ten
plus the next two rolls; a spare (all ten pins with both balls) scores ten plus the next roll. In
frame ten a strike or a spare grants bonus balls, and `frame_ten` adds them to the game.

## Handicap

The handicap levels the field: `compute_hdcp` computes it as (200 - average) * 0.9, rounded down,
the USBC rule. A series is three games; `series_total` adds the games and the handicap.

Run the tests with `make test`.
