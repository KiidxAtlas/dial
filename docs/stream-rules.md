# Stream rules

Stream rules are portable guidance files that watch the model's output while it streams. When a
rule's condition matches, Dial either stops the inference and regenerates it with the rule's
guidance, or lets the reply finish and reminds the next request. Rules are model context only:
they never run commands, change permissions or override configuration.

## Where rules are loaded from

Low to high precedence, user then project:

- User: `~/.copilot/copilot-instructions.md`, `~/.cursor/rules/`, Copilot directories listed in
  `COPILOT_CUSTOM_INSTRUCTIONS_DIRS`, Windsurf global rules when `context_files.foreign_sources`
  includes `windsurf`, and `~/.dial/rules/`.
- Project: `.github/copilot-instructions.md`, `.github/instructions/`, `.cursor/rules/`,
  `.windsurf/rules/`, `.windsurfrules`, the nearest `.clinerules`, and `.dial/rules/`.

A project rule replaces a user rule with the same relative name from the same provider. Files that
cannot be parsed are refused with a warning; a refused file never loads partially.

## Rule format

A rule is a Markdown file whose frontmatter selects when it applies; the body is the guidance the
model receives.

```markdown
---
condition: "\\brm -rf\\b"
scope: tool:bash
interruptMode: always
---
Never delete recursively; move files to a scratch directory instead.
```

Frontmatter is a strict YAML subset (strings, booleans, inline or `- item` string arrays). Any
other field is refused.

| Field | Meaning |
| --- | --- |
| `condition` | Rust regex, or a list of them (any matches). Makes the rule a stream rule. |
| `astCondition`, `astLanguage` | ast-grep pattern(s) matched against finalized source-bearing tool arguments ([structural guidance](extensibility.md#structural-guidance)). |
| `scope` | Where `condition` looks: `text`, `thinking`, `tool:NAME` or `tool:NAME(GLOB)`. Defaults to text, thinking and every tool. |
| `interruptMode` | Which matches stop the inference (below). Default `always`. |
| `question` | A yes/no question the judge role answers about each completed reply (below). |
| `globs` / `applyTo` | Paths the rule applies to; GitHub instructions require `applyTo`. |
| `alwaysApply` | `true` keeps the rule in context on every request. |
| `agents` | Agent names the rule applies to (`main`, worker names; globs allowed). |
| `enabled` | `false` skips the file. |
| `name`, `description` | Informational. |

## Interrupt modes

| `interruptMode` | Interrupts on | Other matches |
| --- | --- | --- |
| `always` | text, reasoning and tool calls | — |
| `prose-only` | text and reasoning | tool-call matches are deferred |
| `tool-only` | tool calls | text and reasoning matches are deferred |
| `never` | nothing | every match is deferred |

An interrupting match aborts the inference before any tool runs and regenerates it with the rule's
guidance, up to `portable_rules.max_regenerations` times per turn. A deferred match keeps the reply
and its tool calls; the rule's reminder reaches the next request instead.

Each firing shows a notice in the conversation (`…; regenerating with its guidance` or
`…; its reminder reaches the next request`).

## Judged rules

A rule with `question` is not matched while streaming. After a reply completes, the judge role is
asked the question about the reply's in-scope content and answers `yes` or `no`; `yes` fires the
rule as a deferred reminder. A `condition` or `astCondition` on the same rule acts as a prefilter:
only replies it matches are judged. Judged rules never interrupt, so `interruptMode` other than
`always` is refused on them.

`portable_rules.judge` controls the cost:

- `auto` (default): judge only when the `judge` role is explicitly assigned in `[model_roles]`, so
  no main-model calls are spent without a deliberate choice.
- `on`: judge through the judge role, falling back to the `small` role and then the main model.
- `off`: never judge.

A failed judgment is reported as a diagnostic and does not fire the rule.

## Repeat policy

A rule fires at most once per session by default. `portable_rules.repeat_after_turns = N` lets a
fired rule fire again after `N` further user turns; `0` keeps it quiet for the rest of the session.

## Configuration

```toml
[portable_rules]
enabled = true              # load rules at all
max_regenerations = 8       # interrupting regenerations per turn (1..64)
repeat_after_turns = 0      # 0 = once per session; N = may fire again after N user turns (<= 1000)
judge = "auto"              # auto | on | off
```

## Authoring and testing

`/rule <complaint>` drafts a stream rule from a complaint about the session's output, checks it
against the assistant output so far and saves it to the project or user rules directory. Judged
`question` rules are written by hand.

`dial rules list` shows each loaded rule with its triggers, interrupt mode, scopes and globs, and
every refused file with the reason. `dial rules test` runs one sample through the stream rules
exactly as a live inference would, without a model:

```sh
dial rules test 'TODO: rm -rf build'                     # assistant text (`-` reads stdin)
dial rules test --reasoning 'plan: skip the tests'       # model reasoning
dial rules test --tool bash --args '{"command":"rm -rf x"}'
dial rules --json test --agent reviewer 'looks good'
```

It prints whether each matching rule interrupts or defers, and which judged rules would be asked of
the judge role (they are not asked). The exit status is 1 when anything fires, 0 otherwise.
