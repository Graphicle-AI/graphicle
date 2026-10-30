#!/usr/bin/env python3
"""lines.py: hunks placed on S, and E1L's gold-line rule on the placed hunks. Run: python3 test_lines.py"""
import os
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import lines  # noqa: E402


def section(body):
    return ["diff --git a/f.py b/f.py", "--- a/f.py", "+++ b/f.py"] + body


class Place(unittest.TestCase):
    S = [f"line {i}" for i in range(1, 21)]

    def test_a_hunk_written_against_an_older_base_moves_to_its_lines_at_s(self):
        # The pull request's base had three lines fewer above: its header says 4, S has them at 7.
        sec = section(["@@ -4,3 +4,3 @@", " line 7", "-line 8", "+LINE 8", " line 9"])
        placed, offsets = lines.place(sec, self.S)
        self.assertEqual(offsets, [3])
        gold, minus, hunks = lines.gold_lines(placed)
        self.assertEqual((gold, minus, hunks), ([8], {8: "line 8"}, 1))

    def test_an_insertion_without_minus_lines_gives_the_line_before_it(self):
        sec = section(["@@ -10,2 +10,3 @@", " line 12", "+new", " line 13"])
        placed, offsets = lines.place(sec, self.S)
        self.assertEqual(offsets, [2])
        self.assertEqual(lines.gold_lines(placed)[0], [12])

    def test_the_nearest_match_after_the_previous_hunk_wins(self):
        s = ["x", "a", "b", "x", "a", "b", "x"]
        sec = section(["@@ -5,2 +5,2 @@", "-a", "+A", " b"])
        placed, offsets = lines.place(sec, s)
        self.assertEqual(offsets, [0])
        self.assertEqual(lines.gold_lines(placed)[0], [5])

    def test_a_hunk_that_matches_nowhere_is_not_placed(self):
        sec = section(["@@ -4,2 +4,2 @@", " line 4", "-not at S", "+x"])
        placed, why = lines.place(sec, self.S)
        self.assertIsNone(placed)
        self.assertIn("does not match", why)

    def test_carriage_returns_do_not_stop_a_match(self):
        sec = section(["@@ -2,1 +2,1 @@", "-line 2\r", "+LINE 2\r"])
        placed, offsets = lines.place(sec, self.S)
        self.assertEqual((offsets, lines.gold_lines(placed)[0]), ([0], [2]))


if __name__ == "__main__":
    unittest.main()
