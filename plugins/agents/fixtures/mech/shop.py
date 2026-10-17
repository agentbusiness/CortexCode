from calc import price


def total(items):
    return sum(price(item, 1) for item in items)


def receipt(items):
    lines = [f"{item}: {price(item, 2)}" for item in items]
    return "\n".join(lines)
