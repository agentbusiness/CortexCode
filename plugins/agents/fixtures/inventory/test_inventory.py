import unittest

from inventory import Inventory, OutOfStock


class InventoryTest(unittest.TestCase):
    def setUp(self):
        self.inventory = Inventory()
        self.inventory.add("apple", 3)
        self.inventory.add("pear", 2)

    def test_count(self):
        self.assertEqual(self.inventory.count("apple"), 3)
        self.assertEqual(self.inventory.count("plum"), 0)

    def test_remove(self):
        self.inventory.remove("apple", 2)
        self.assertEqual(self.inventory.count("apple"), 1)

    def test_remove_too_many(self):
        with self.assertRaises(OutOfStock):
            self.inventory.remove("pear", 5)

    def test_total_items(self):
        self.assertEqual(self.inventory.total_items(), 5)

    def test_report(self):
        self.assertEqual(self.inventory.report(), "apple=3, pear=2")


if __name__ == "__main__":
    unittest.main()
