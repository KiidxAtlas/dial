import unittest
from snake import advance


class SnakeTest(unittest.TestCase):
    def test_moves_from_head(self):
        body = [(3, 4), (3, 3), (3, 2)]
        self.assertEqual(advance(body, (0, 1)), [(3, 5), (3, 4), (3, 3)])
        self.assertEqual(body, [(3, 4), (3, 3), (3, 2)])

    def test_single_segment(self):
        self.assertEqual(advance([(2, 2)], (-1, 0)), [(1, 2)])


if __name__ == "__main__":
    unittest.main()
