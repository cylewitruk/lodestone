#!/usr/bin/env python3
"""Checks for score.py's shell search detection: `python3 -m unittest test_score`."""
import unittest

from score import is_grep


class ShellSearches(unittest.TestCase):
    def test_counted(self):
        for c in [
            "grep -rn 'a\\|b' src | head",
            "cd x && LC_ALL=C rg foo",
            "git grep -n foo",
            "/usr/bin/grep -r term .",
            "echo start; grep -r foo .",
            "make 2>&1 >/dev/null; grep -r foo .",
            "echo '<<EOF'\nrg needle .",
            "cat <<'EOF' > notes.txt\ngrep = 1\nEOF\nrg needle .",
            "# don't scan generated files\nrg needle src",
            "cat <<< 'note'\nrg needle .",
            "cat <<-EOF\n\tbody\n\tEOF\nrg needle .",
            "echo a \\\n  && rg needle .",
        ]:
            self.assertTrue(is_grep("Bash", {"command": c}), c)

    def test_not_counted(self):
        for c in [
            "printf '%s' 'done; grep foo'",
            "cargo test 2>&1 | grep FAIL",
            "git log --grep foo",
            "python3 - <<'EOF'\ngrep = 1\nEOF",
            "ls",
            "make 2>&1 grep",
            "printf '%s %s' ';' grep",
            "echo \"a | grep b\"",
            "cargo test |\ngrep FAILED",
            "cargo test \\\n  | grep FAILED",
            "cat <<EOF\n  EOF\ngrep = 1\nEOF",
            "echo a#b grep",
        ]:
            self.assertFalse(is_grep("Bash", {"command": c}), c)


if __name__ == "__main__":
    unittest.main()
