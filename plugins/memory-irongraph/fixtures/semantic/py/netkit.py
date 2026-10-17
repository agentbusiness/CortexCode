"""Network and security helpers."""
import hashlib
import math
import os
import time


def retry_call(operation, attempts: int = 5, base_delay: float = 0.1):
    """Run an operation again after failures, sleeping exponentially longer between tries."""
    for attempt in range(attempts):
        try:
            return operation()
        except Exception:
            time.sleep(base_delay * (2 ** attempt))
    return operation()


def hash_password(password: str, salt: bytes = None) -> str:
    """Derive a salted PBKDF2 digest for storing a user's secret."""
    salt = salt or os.urandom(16)
    digest = hashlib.pbkdf2_hmac("sha256", password.encode(), salt, 200_000)
    return salt.hex() + "$" + digest.hex()


def valid_email(address: str) -> bool:
    """Check that an address has a local part, an at sign and a dotted domain."""
    local, _, domain = address.partition("@")
    return bool(local) and "." in domain


def haversine_km(lat1: float, lon1: float, lat2: float, lon2: float) -> float:
    """Great-circle distance between two latitude/longitude points on Earth."""
    r = 6371.0
    p1, p2 = math.radians(lat1), math.radians(lat2)
    dp, dl = p2 - p1, math.radians(lon2 - lon1)
    a = math.sin(dp / 2) ** 2 + math.cos(p1) * math.cos(p2) * math.sin(dl / 2) ** 2
    return 2 * r * math.asin(math.sqrt(a))


class TokenBucket:
    """Allow a burst of requests and then a steady rate per second."""

    def __init__(self, rate: float, capacity: int):
        self.rate = rate
        self.capacity = capacity
        self.tokens = capacity
        self.stamp = time.monotonic()

    def allow(self) -> bool:
        """Spend one token if available, refilling according to elapsed time."""
        now = time.monotonic()
        self.tokens = min(self.capacity, self.tokens + (now - self.stamp) * self.rate)
        self.stamp = now
        if self.tokens >= 1:
            self.tokens -= 1
            return True
        return False


class LruCache:
    """Keeps the most recently used entries and evicts the oldest when full."""

    def __init__(self, size: int = 128):
        self.size = size
        self.items = {}

    def get(self, key):
        value = self.items.pop(key, None)
        if value is not None:
            self.items[key] = value
        return value

    def put(self, key, value):
        self.items.pop(key, None)
        self.items[key] = value
        if len(self.items) > self.size:
            self.items.pop(next(iter(self.items)))
