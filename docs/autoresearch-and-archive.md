# Autoresearch and archive

## Autoresearch

Autoresearch keeps experiment configuration, benchmark output, notes, and run metrics in the project's Dial state directory (`autoresearch/runs.sqlite`), not in the repository. Initialize from a clean Git worktree:

```text
/autoresearch init <name> <metric> <max|min> <benchmark command>
/autoresearch run
/autoresearch auto [iterations] [--patience N]
/autoresearch log
/autoresearch notes [text]
/autoresearch resume
/autoresearch exit
```

`<max|min>` is required: `max` or `min`, case-insensitive. Anything else is a usage error. `init` switches to the experiment branch `autoresearch/<name>` and records the branch (or commit, when detached) you were on, so `/autoresearch exit` can return there. Leaving the experiment keeps its branch, runs and best ref; `init` with the same name resumes it.

`run` benchmarks the working tree but records `HEAD`, so it refuses while the tree has uncommitted or untracked changes. Commit them, or stash them with `git stash push -u`, then run again. The check protects the rule that the recorded commit is the one measured. `exit` also needs a clean tree.

The benchmark command runs inside Dial's strict workspace sandbox; it has no network access. Emit the selected metric as `metric=value`, `ASI metric=value`, or a JSON object containing the metric. Non-finite values are rejected. Runs store the current commit and full output, and the current best metric is marked in the run log. The best is tracked per experiment and recorded as the Git ref `autoresearch/<name>-best`. Use the Git branch created for the experiment as the boundary for manual code changes.

### Self-driving loop

`/autoresearch auto` lets the agent iterate on its own. It first measures the unchanged branch when no run has been kept yet, so every candidate must beat the starting code. Then each iteration:

1. Dial records the commit at the start and sends the agent a prompt with the goal, the best metric, the five most recent runs and your notes. It asks for one focused change, and no commits, branch switches, benchmark edits or metric gaming.
2. When the turn ends, Dial commits whatever changed on the experiment branch, runs the benchmark, and keeps the commit only if it beats the best run (moving `autoresearch/<name>-best`). A run that is no better, a failed benchmark, or output without the metric resets the branch to the iteration's start; the reverted commit stays reachable through the reflog. An unchanged tree is recorded as "no change".

The loop stops after `iterations` (default 10, at most 100), after `--patience` iterations in a row without improvement (default 3), when a turn ends waiting for you (a question, failure or Esc), or when the working tree is no longer on the experiment branch. The keep or revert decision is always Dial's, never the agent's. Each iteration and the final best metric are reported in the transcript, and `/autoresearch log` lists the runs.

## Archive

Archive reads recent projects and bounded session previews from Dial state directories, and reads deduplicated submitted prompts and journaled idle recaps from the user's `history.db`. The `archive` tool is read-only; cross-project history requires approval. `/archive` browses recent projects, sessions, and prompt history. Prompt history is global across projects; repeated normalized prompts update recency and use count rather than creating duplicates.

A single-session view (`history source=sessions action=session id=<prefix>`) also lists that session's failures, oldest first: tool calls that returned an error (including harness refusals such as a changed tool contract), with the call's clipped arguments, and turns that ended failed or interrupted, with their error. Recovery notices and retry hints are stripped so the cause stays visible. The transcript is streamed line by line, so any size is covered; `limit` keeps the newest failures and the section heading reports the total found.
