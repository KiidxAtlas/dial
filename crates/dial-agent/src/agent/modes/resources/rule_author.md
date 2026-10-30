<rule-author>
The user is frustrated about recurring agent behavior.
Author ONE stream rule that would have caught the offending behavior earlier in this conversation.

Stream rule mechanics:
- A rule is a markdown file with YAML frontmatter. Its body is injected as correction guidance when the rule triggers.
- `scope` is an allowlist of checked outputs, given as a string or an array:
  - `text` = assistant prose. `thinking` = reasoning summaries. `tool:*` = every tool call.
  - `tool:<name>(<glob>)` = one tool, only when its file path matches the glob, e.g. `tool:write(*.rb)`, `tool:edit(*.ts)`.
- The trigger is checked against the scoped outputs the agent produces.

Trigger:
- `condition`: regex pattern(s), given as a string or an array of strings. Use literal tokens or phrases, e.g. `: any|as any`.
- `astCondition`: ast-grep code pattern(s) for finalized source-bearing tool arguments. Use narrow tool/file scopes; `astLanguage` optionally selects a parser such as rust or typescript. It does not inspect files or shell commands.
- Provide condition, astCondition, or both. NEVER emit question; judged rules are written by hand, not generated here.

Output contract:
- Emit exactly one JSON object and nothing else.
- JSON fields: `name`, `description`, `scope`, `body`, and condition and/or astCondition; astLanguage is optional.
- `name` MUST be kebab-case.
- `description` MUST be a one-line summary.
- condition and astCondition MUST be strings or string arrays. AST triggers require a tool scope.
- Triggers MUST catch the specific offending assistant output visible earlier in this conversation.
- Escape regex backslashes for JSON exactly once: use `"\\beval\\s*\\("`, NEVER `"\\\\beval\\\\s*\\\\("`.
- Keep triggers precise; NEVER use broad catch-alls.
- `scope` MUST be a string or string array, as narrow as the complaint allows. NEVER use `tool:*` or `text` unless the same bad behavior occurred in both tool calls and prose.
- Code complaints SHOULD use file-specific tool scopes: Ruby written through `write` → `tool:write(*.rb)`, not `tool:*` or `text`.
- `body` MUST be markdown guidance explaining the right behavior concisely.
- The caller assembles YAML frontmatter. NEVER emit markdown frontmatter or a fenced code block around the JSON.

Example shapes:
{
  "name": "ts-no-any",
  "description": "Never use `any` in TypeScript — use `unknown`, a generic, or the real type",
  "condition": ": any|as any",
  "scope": ["tool:edit(*.ts)", "tool:edit(*.tsx)", "tool:write(*.ts)", "tool:write(*.tsx)"],
  "body": "Never use `: any` or `as any`. Use `unknown`, a domain type, a generic, or a type guard."
}
{
  "name": "no-unverified-test-claims",
  "description": "Never claim tests pass without having run them",
  "condition": "(?i)tests? (pass|are passing|succeed)",
  "scope": "text",
  "body": "Only report test results you observed. Run the tests, or say they were not run."
}

Complaint:
{{complaint}}
{{feedback}}
</rule-author>
