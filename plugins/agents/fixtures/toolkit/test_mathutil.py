import unittest

from mathutil import clamp, mean


class MathTest(unittest.TestCase):
    def test_mean(self):
        self.assertEqual(mean([1, 2, 3, 4]), 2.5)
        self.assertEqual(mean([5]), 5)

    def test_clamp(self):
        self.assertEqual(clamp(15, 0, 10), 10)
        self.assertEqual(clamp(-1, 0, 10), 0)


if __name__ == "__main__":
    unittest.main()
