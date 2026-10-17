"""Small text helpers."""
import re
import unicodedata


def slugify(title: str) -> str:
    """Turn a title into a lowercase URL path segment with hyphens."""
    normalized = unicodedata.normalize("NFKD", title).encode("ascii", "ignore").decode()
    return re.sub(r"[^a-z0-9]+", "-", normalized.lower()).strip("-")


def word_count(text: str) -> int:
    """Number of whitespace separated words."""
    return len(text.split())


def truncate_ellipsis(text: str, limit: int = 80) -> str:
    """Shorten text to a maximum length, ending with three dots when cut."""
    return text if len(text) <= limit else text[: limit - 3] + "..."


def is_palindrome(text: str) -> bool:
    """True when the letters read the same forwards and backwards."""
    letters = [c.lower() for c in text if c.isalnum()]
    return letters == letters[::-1]


def levenshtein(a: str, b: str) -> int:
    """Edit distance: the fewest insertions, deletions and substitutions turning a into b."""
    previous = list(range(len(b) + 1))
    for i, ca in enumerate(a, 1):
        current = [i]
        for j, cb in enumerate(b, 1):
            current.append(min(previous[j] + 1, current[j - 1] + 1, previous[j - 1] + (ca != cb)))
        previous = current
    return previous[-1]


def parse_csv_line(line: str, delimiter: str = ",") -> list:
    """Split one comma separated record into fields, honouring double quotes."""
    fields, current, quoted = [], "", False
    for ch in line:
        if ch == '"':
            quoted = not quoted
        elif ch == delimiter and not quoted:
            fields.append(current)
            current = ""
        else:
            current += ch
    fields.append(current)
    return fields
