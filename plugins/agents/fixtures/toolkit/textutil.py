"""Text helpers."""

import re


def slugify(text):
    """Lower-case words joined by single hyphens: "Hello, World!" -> "hello-world"."""
    words = re.findall(r"[A-Za-z0-9]+", text)
    return "-".join(words)


def truncate(text, width):
    """Cut text to at most `width` characters, ending with an ellipsis when cut."""
    if len(text) <= width:
        return text
    return text[: width - 1] + "…"
