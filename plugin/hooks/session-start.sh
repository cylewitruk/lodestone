#!/bin/sh
# The lodestone reminder, added only when a lodestone daemon serves the session's repository
# (`lode hook session-start`; LODESTONE_PORT overrides the configured port).
command -v lode >/dev/null 2>&1 || exit 0
exec lode hook session-start
