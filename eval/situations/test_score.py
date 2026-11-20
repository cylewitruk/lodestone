#!/usr/bin/env python3
"""Checks score.py's shell search detection against the cases `src/shell.rs` checks too:
`python3 -m unittest test_score`."""
import json
import unittest
from pathlib import Path

from score import is_grep

CASES = json.loads(
    (Path(__file__).resolve().parents[2] / "tests/fixtures/shell-searches.json").read_text()
)


class ShellSearches(unittest.TestCase):
    def test_searches(self):
        for c in CASES["search"]:
            self.assertTrue(is_grep("Bash", {"command": c}), c)

    def test_no_searches(self):
        for c in CASES["no_search"]:
            self.assertFalse(is_grep("Bash", {"command": c}), c)


if __name__ == "__main__":
    unittest.main()
