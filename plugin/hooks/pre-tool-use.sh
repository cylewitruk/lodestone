#!/bin/sh
# Points a shell search by guessed names in an indexed repository at lodestone's search
# (`lode hook pre-tool-use`). Never fails: exit status 2 would block the command, and a `lode`
# too old to know the subcommand exits with it.
command -v lode >/dev/null 2>&1 || exit 0
lode hook pre-tool-use || exit 0
