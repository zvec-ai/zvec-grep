import unittest

from sample import distribution, select


class SampleTest(unittest.TestCase):
    def test_determinism_coverage_and_byte_balance(self):
        rows = [
            {"path": f"{i}.md", "bytes": i * i + 1, "sha256": "unused"}
            for i in range(1000)
        ]
        chosen, attempt = select(rows, 64)
        self.assertEqual((chosen, attempt), select(list(reversed(rows)), 64))
        self.assertEqual(len({r["path"] for r in chosen}), 64)
        for i, row in enumerate(chosen):
            self.assertIn(row, rows[i * 1000 // 64 : (i + 1) * 1000 // 64])
        full, partial = distribution(rows), distribution(chosen)
        self.assertLess(abs(partial["mean_bytes"] / full["mean_bytes"] - 1), 0.01)

    def test_invalid_sample_size(self):
        with self.assertRaises(AssertionError):
            select([], 1)


if __name__ == "__main__":
    unittest.main()
