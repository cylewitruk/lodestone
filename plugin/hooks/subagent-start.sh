#!/bin/sh
# The lodestone reminder for a subagent, which sees none of its parent session's
# (`lode hook subagent-start`).
command -v lode >/dev/null 2>&1 || exit 0
exec lode hook subagent-start
