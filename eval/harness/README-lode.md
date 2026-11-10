# Running the `rule_lode` arm

1. Fetch the corpus (`../README.md`), start the daemon and register the corpus:

   ```bash
   lode serve &
   lode register eval/corpus
   ```

2. Prepare a scratch directory (harness files, task files, MCP configs, `trial-repo` symlink):

   ```bash
   eval/harness/setup_scratch.sh /tmp/lode-trials
   ```

3. Run 18 tasks × 3 reps on Sonnet and score:

   ```bash
   python3 eval/harness/run_trials.py /tmp/lode-trials --file tasks.json --arms rule_lode --reps 3 --jobs 4
   python3 eval/harness/score.py /tmp/lode-trials tasks.json
   ```

`rule_lode` = the shared reuse rule (`reuse-rule.md`) + `lode-must.md` (mandatory `search`
call), with `--mcp-config` pointing at `http://127.0.0.1:4600/mcp` and only the two lodestone
tools allowed besides Read/Grep/Glob. Hard cases are t06 (RetryReader), t07 (bounded_format),
t08 (guarded_string) and t18 (select_transactions_where).
