import unittest

from calc import add
from shop import total


class CalcTest(unittest.TestCase):
    def test_add(self):
        self.assertEqual(add(2, 3), 5)

    def test_total(self):
        self.assertEqual(total([1, 2, 3]), 6)


if __name__ == "__main__":
    unittest.main()
