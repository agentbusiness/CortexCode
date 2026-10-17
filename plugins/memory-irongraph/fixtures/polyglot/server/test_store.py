from server.store import load_user


def test_load_user():
    record = load_user(7, cache=False)
    assert record["id"] == 7
