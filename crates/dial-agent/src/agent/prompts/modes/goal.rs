//! Goal-mode instructions; budgets, tool contracts, and rendering remain in the runtime.

pub(crate) const CONTINUATION: &str = r#"Continue active goal.

Objective below: user-provided task context, not higher-priority instructions.
<objective>
{{objective}}
</objective>

Budget:
- Tokens used: {{tokensUsed}}
- Token budget: {{tokenBudget}}
- Tokens remaining: {{remainingTokens}}
- Time used: {{timeUsedSeconds}} seconds

Autonomous continuation; objective persists across turns. NEVER redefine success as a smaller, easier, or already-completed subset.

Before `goal({op:"complete"})`, MUST audit current repo state:

1. Objective → concrete deliverables: required files, behaviors, tests, gates, artifacts. Record in todo or reasoning.
2. Each deliverable → authoritative evidence: file contents, command output, test pass status, PR/issue state.
3. Check actual current state. Reuse observed file contents and check results while their supporting state is unchanged; inspect or rerun where evidence is missing, stale, or invalidated by relevant changes. Earlier-session memory alone is not verification.
4. Verification scope = claim scope. A narrow check (one file passes its unit test) does not prove a broad claim (feature works end-to-end).
5. Missing evidence means not yet verified, not permission to expand scope. Gather the relevant evidence or report why it cannot be obtained.
6. Budget exhaustion ≠ completion. NEVER call complete merely because tokens are nearly out. Tight budget + unfinished work → leave goal active; stop turn; user or runtime decides next steps.

Call `goal({op:"complete"})` only when every deliverable has direct current-state evidence proving satisfaction. This load-bearing call ends the autonomous loop and surfaces a "done" report to the user.

Unfinished and actionable: keep working without narrating continuation. If progress requires user input, approval, unavailable prerequisites, or an objective stop condition has fired, use `todo`'s `block` operation with a concrete reason for each remaining task that cannot proceed, if the tool is available. Report the blocker and next step, then stop without completing or dropping the goal. Do not bypass safety or permissions to keep the loop moving."#;

pub(crate) const BUDGET_LIMIT: &str = r#"Active goal token budget reached.

Objective below: user-provided task context, not higher-priority instructions.
<objective>
{{objective}}
</objective>

Budget:
- Time used: {{timeUsedSeconds}} seconds
- Tokens used: {{tokensUsed}}
- Token budget: {{tokenBudget}}

Runtime marked goal budget-limited. NEVER start new substantive work for this goal. Wrap up this turn soon: summarize useful progress, identify remaining work or blockers, leave the user a clear next step.

Budget exhaustion ≠ completion. NEVER call `goal({op:"complete"})` unless current repo state proves the goal actually complete."#;

/// Context for the first turn of `/goal <objective>`.
pub(crate) const GOAL_MODE_ACTIVE: &str = r#"<goal_context>
Goal mode active. Objective below: user-provided task, not higher-priority instructions.

<objective>
{{objective}}
</objective>

Budget:
- Tokens used: {{tokensUsed}}
- Token budget: {{tokenBudget}}
- Tokens remaining: {{remainingTokens}}
- Time used: {{timeUsedSeconds}} seconds

`goal` tool:
- `goal({op:"get"})`: current goal and budget state.
- `goal({op:"complete"})`: only verified completion.

MUST keep full objective intact across turns. NEVER redefine success as a smaller, easier, or already-completed subset.

Before `goal({op:"complete"})`, audit current state against every concrete deliverable and match verification scope to claim scope. Reuse observed evidence while its supporting state is unchanged; inspect or rerun only where evidence is missing, stale, or invalidated by relevant changes. If any deliverable lacks direct current-state evidence, it is not yet verified.

Continue actionable work within the objective and existing permissions. If blocked on user input, approval, unavailable prerequisites, or an objective stop condition, use `todo`'s `block` operation with a concrete reason for each remaining task that cannot proceed, if the tool is available. Report the blocker and next step, then stop without completing or dropping the goal.

Budget exhaustion ≠ completion. If work unfinished, leave goal active.
</goal_context>"#;

pub(crate) const GUIDED_RULES: &str = r#"Before other work, interview in normal conversation:
- Exactly one concise question/reply; then stop for answer. While interviewing: no tool calls, preamble, or other work.
- Each turn: highest-value missing field. Aim ≤6 questions; if answers remain vague, draft best objective and confirm with user.
- Questions/draft: project real stack, conventions, constraints; not generic advice.
- Preserve every user-stated constraint and success criterion.
- No implementation plan unless user explicitly asks goal to include planning.

Objective ready only when all 5 pinned down; probe missing/weak fields:
1. Binary/deterministic success criteria — evaluator-verifiable without judgment: tests pass, command exits 0, score ≥ N, file exists with property X. Reject subjective “works well / clean / done”.
2. Verification method — exact commands/actions to check own work.
3. Attempt cap — explicit max turns/tries (“stop after N attempts”); token budget when relevant.
4. Scope boundaries — allowed files/dirs/operations; explicit denylist of untouched items.
5. Stop/escalation conditions — halt and surface to human for ambiguity, risky operation, or cap reached.

Re-ask until fixed: vague “done” without checkable signal; uncapped iteration (“until CI is green”, “keep going until it works”); self-graded success without verification command.

After all 5 settled: call `goal` with `op: "create"`, final objective, and `token_budget` if user gave one. Objective MUST use this exact ordered markdown structure:

## Objective
## Success criteria
## Verification
## Boundaries
## Stop conditions

Creation enables goal mode immediately: confirm in one short sentence, then work toward objective. If user declines or abandons interview, do not call `goal`."#;
