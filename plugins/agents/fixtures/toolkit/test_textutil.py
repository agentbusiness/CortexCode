import unittest

from textutil import slugify, truncate


class TextTest(unittest.TestCase):
    def test_slugify(self):
        self.assertEqual(slugify("Hello, World!"), "hello-world")
        self.assertEqual(slugify("  Many   spaces here "), "many-spaces-here")

    def test_truncate(self):
        self.assertEqual(truncate("short", 10), "short")
        self.assertEqual(truncate("abcdefghij", 5), "abcd…")


if __name__ == "__main__":
    unittest.main()
