#!/bin/sh
# The lodestone reminder, added only when a lodestone daemon serves the session's repository
# (`lode hook session-start`; LODESTONE_SOCKET overrides the configured socket).
command -v lode >/dev/null 2>&1 || exit 0
exec lode hook session-start
