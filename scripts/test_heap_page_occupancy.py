import unittest

from heap_page_occupancy import summarize


class OccupancyTests(unittest.TestCase):
    def test_cross_page_blocks_and_shared_page_holes(self):
        result = summarize("0x8: non-object (16 bytes)\n0x1c: non-object (4 bytes)", 16)
        self.assertEqual(result["allocated_bytes"], 20)
        self.assertEqual(result["pages_with_live_blocks"], 2)
        self.assertEqual(result["outside_block_bytes"], 12)
        self.assertEqual(result["occupancy"]["25-50%"]["pages"], 1)
        self.assertEqual(result["occupancy"]["50-75%"]["pages"], 1)

    def test_page_boundary_and_empty_pages(self):
        result = summarize("0x0: non-object (16 bytes)\n0x30: non-object (16 bytes)", 16)
        self.assertEqual(result["pages_with_live_blocks"], 2)
        self.assertEqual(result["outside_block_bytes"], 0)

    def test_reject_incomplete_or_overlapping_input(self):
        for text in ("summary only", "0x0: non-object (16 bytes)\n0x8: non-object (16 bytes)"):
            with self.assertRaises(ValueError):
                summarize(text, 16)


if __name__ == "__main__":
    unittest.main()
