import os

MAX_USERS = 100


class UserStore:
    """Keeps user records on disk."""

    def __init__(self, path: str = "users.db"):
        self.path = path
        self.cache = {}

    def load(self, user_id: int, *, fresh: bool = False) -> dict:
        """Read one user."""
        if fresh or user_id not in self.cache:
            self.cache[user_id] = {"id": user_id, "path": self.path}
        return self.cache[user_id]


class AuditedStore(UserStore):
    """A store that counts reads."""

    def load(self, user_id: int, *, fresh: bool = False) -> dict:
        self.reads = getattr(self, "reads", 0) + 1
        return super().load(user_id, fresh=fresh)


def load_user(user_id: int, cache: bool = True) -> dict:
    store = UserStore(os.environ.get("USER_DB", "users.db"))
    if user_id > MAX_USERS:
        raise LookupError(user_id)
    return store.load(user_id, fresh=not cache)
