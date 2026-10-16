# Autoresearch and archive

## Autoresearch

Autoresearch keeps experiment configuration, benchmark output, notes, and run metrics in the project's Dial state directory (`autoresearch/runs.sqlite`), not in the repository. Initialize from a clean Git worktree:

```text
/autoresearch init <name> <metric> <max|min> <benchmark command>
/autoresearch run
/autoresearch log
/autoresearch notes [text]
/autoresearch resume
/autoresearch exit
```

`<max|min>` is required: `max` or `min`, case-insensitive. Anything else is a usage error. `init` switches to the experiment branch `autoresearch/<name>` and records the branch (or commit, when detached) you were on, so `/autoresearch exit` can return there. Leaving the experiment keeps its branch, runs and best ref; `init` with the same name resumes it.

`run` benchmarks the working tree but records `HEAD`, so it refuses while the tree has uncommitted or untracked changes. Commit them, or stash them with `git stash push -u`, then run again. The check protects the rule that the recorded commit is the one measured. `exit` also needs a clean tree.

The benchmark command runs inside Dial's strict workspace sandbox; it has no network access. Emit the selected metric as `metric=value`, `ASI metric=value`, or a JSON object containing the metric. Non-finite values are rejected. Runs store the current commit and full output, and the current best metric is marked in the run log. The best is tracked per experiment and recorded as the Git ref `autoresearch/<name>-best`. Use the Git branch created for the experiment as the boundary for manual code changes.

## Archive

Archive reads recent projects and bounded session previews from Dial state directories, and reads deduplicated submitted prompts and journaled idle recaps from the user's `history.db`. The `archive` tool is read-only; cross-project history requires approval. `/archive` browses recent projects, sessions, and prompt history. Prompt history is global across projects; repeated normalized prompts update recency and use count rather than creating duplicates.
