from mypkg import slugify


def test_slugify_joins_words():
    assert slugify("Hello, World!") == "hello-world"


def test_slugify_empty():
    assert slugify("  ") == ""
