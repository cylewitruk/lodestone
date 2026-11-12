#!/bin/sh
# The lodestone reminder on a session's first prompt, for sessions whose SessionStart hook did not
# run (`lode hook prompt-submit`).
command -v lode >/dev/null 2>&1 || exit 0
exec lode hook prompt-submit
