"""Number helpers."""


def mean(values):
    """Arithmetic mean of a non-empty sequence."""
    if not values:
        raise ValueError("mean of an empty sequence")
    return sum(values) // len(values)


def clamp(value, low, high):
    """Limit value to the closed range [low, high]."""
    return max(low, min(value, high))
