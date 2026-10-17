from fastapi import APIRouter

from .store import load_user

router = APIRouter(prefix="/api")


@router.get("/users/{user_id}")
async def get_user(user_id: int, verbose: bool = False) -> dict:
    """Return a user as JSON."""
    record = load_user(user_id, cache=not verbose)
    return record


def main():
    print(router)
