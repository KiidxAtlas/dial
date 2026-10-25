# Writing and evaluating skills

A Dial **skill is an on-demand Markdown procedure**, not an executable tool, command handler,
permission grant or subagent. Use a native tool/extension when the feature needs enforced
parameters, structured results or execution logic; use a skill to explain how to combine existing
capabilities for a repeatable task. Guidance does not enforce approvals, verification or safety.

## Locations and ownership

| Location | Responsibility |
| --- | --- |
| `.dial/skills/<name>/SKILL.md` | Active project-authored/imported procedures; ignored local state in this repository |
| `$DIAL_HOME/skills/<name>/SKILL.md` (normally `~/.dial/skills`) | User procedures shared across projects |
| `examples/skills/<name>/SKILL.md` | Versioned, portable examples maintained by this repository; not automatically enabled |
| `crates/dial-storage/src/storage/skills.rs` | Metadata parsing, discovery, catalog/hints, `skill` and `skill_manage` tool contracts |
| `crates/dial-storage/src/storage/skills/` | Copy imports, enablement, package sources, fetch and store lifecycle, sibling tests |
| `src/cmd/skills.rs` | CLI wiring to the shared management implementation |

Discovery precedence is Dial project skills, Dial user skills, installed packages, then supported
foreign project/user roots. Supported foreign roots are declared once in
`crates/dial-storage/src/storage/foreign.rs`. Private learned skills are integrated through
`storage::memory_backend`; ordinary authored skills win same-name collisions. Toggles are by name.
Run `dial skills list` to inspect effective locations and enablement in your environment; do not
assume every machine has the same catalog. See [extensibility](extensibility.md#discover-import-and-toggle-skills).

Only names and brief descriptions enter the initial catalog. The agent loads a body through
`skill` and requests bundled files separately. `/skill:<name>` requests an enabled skill in chat;
`/skills` manages the inventory. There is no `/skills test` execution API.

## Authoring contract

Use `<name>/SKILL.md` with a closed `---` frontmatter block. For newly authored skills, include:

- `name`: match the directory, lowercase ASCII letters/digits/hyphens, starting with a letter or
  digit, at most 64 characters. Prefer clear task names; avoid repeated/trailing hyphens.
- `description`: what it does and **when to use it**, ideally under 200 characters so the catalog
  does not truncate the trigger. The parser allows up to 1024 description characters.
- Body: a concise, actionable procedure, not claims of capabilities that do not exist.

Dial accepts a small YAML subset, not arbitrary YAML execution or configuration. Optional absent
`name` values in imported skills retain compatibility; an explicit mismatch is invalid. Unknown
metadata may be preserved, but keys such as `allowed-tools` do not confer permissions or install
dependencies. Do not rely on them as enforcement. `skill_manage` takes `name`, `description` and
Markdown `body` separately; do not pass frontmatter in its body.

### Recommended pattern

Use **Outcome → Scope → Workflow → Verification → Report**. These headings are a writing convention,
not a new parser requirement; compatible third-party skills are not rejected for different prose.

1. **Outcome:** one concrete deliverable.
2. **Scope:** positive triggers, non-goals, read-only versus implementation mode, and handoff to a
   better-matched skill. Do not make every tiny task load multiple procedures.
3. **Workflow:** ordered actions with decision points, existing owners/tools and prerequisites.
   Include failure recovery where relevant. Avoid unconditional rereads, retries, delegation or
   broad test runs; each step should resolve a question or advance the deliverable.
4. **Verification:** observable success criteria, important errors/boundaries, appropriate checks
   and how to report unavailable dependencies. Guidance is not proof of execution.
5. **Report:** useful output, evidence, uncertainty and stop criteria. Do not require a particular
   persona, invented client history, fabricated scores or a structured API that does not exist.

Keep catalog descriptions specific. Keep bodies short enough to read in one load; put genuinely
optional references, scripts and templates in bundled files. Read those only when needed. Review
scripts before execution; a bundle is data, not authorization. Reuse the tool's actual schema and
project checks instead of copying them into every skill. Do not store secrets, private payloads,
bidi/zero-width controls or session progress in reusable procedures.

### Complete example

```markdown
---
name: review-patch
description: Review a patch for concrete correctness, compatibility and security regressions without editing source.
---

# Review patch

## Outcome
Produce actionable findings supported by the changed code and its consumers.

## Scope
Use for a requested review, not implementation or a repository-wide cleanup.

## Workflow
1. Establish the requested diff and behavior to preserve; read relevant project conventions.
2. Trace changed contracts to callers and tests. Distinguish concrete defects from style preferences.
3. For each suspected defect, identify its trigger and consequence. Use a focused check only when
   it resolves uncertainty. Do not edit production code or broaden scope without authorization.

## Verification
Cite inspected locations and actual check outcomes. Do not present static review as runtime proof;
report unavailable services and remaining uncertainty.

## Report
Prioritize concrete findings with severity, location and consequence. A clean review is valid.
```

## Import and maintain the examples

From the repository root:

```sh
dial skills import ./examples/skills --dry-run
dial skills import ./examples/skills
dial skills list
```

For imports already originating from that same directory, use `--update --dry-run`, then `--update`.
The importer refuses local changes and source-identity changes; inspect each report, because a batch
can contain both imports and skipped entries. Existing imports from other tools are not silently
replaced by these examples. Back up/reconcile edits before choosing a replacement; do not delete
working skills merely to make an import succeed. Import metadata records provenance, not proof of
quality. Locally edited imports intentionally retain their original hashes, so later updates detect
and protect those changes.

Bodies returned by `skill` are capped at 64 KiB; oversized externally supplied bodies report
truncation at a UTF-8 boundary. A full root `SKILL.md` can use another 16 KiB for metadata (which must
fit in discovery's first 16 KiB), so a body created at the supported limit remains loadable and
importable. Bundled-file reads, including explicit `file: "SKILL.md"` reads, remain limited to
64 KiB. Local imports allow 200 files, 8 MiB per skill and five nested directories; package-copy
limits differ as documented in [extensibility](extensibility.md#context-and-skills). These are ceilings,
not writing targets: the maintained example bodies are only a few KiB.

A package update validates source fields before invoking Git. New local installs persist canonical
repository paths, so a user-scoped package does not switch sources when another project updates it.
Old relative lock entries have no recoverable origin directory: review their resolved source before
updating, or reinstall explicitly from the intended absolute source. Update checks the stored content
manifest even at the same commit and repairs missing or modified content from the fetched checkout;
treat the package store as managed data, not a place for local authoring.

## Assess utility without inventing popularity

Evaluate each skill against these questions, recording evidence and uncertainty rather than a
synthetic numeric score:

- **Coverage:** Which recurring user task does it support? What does success look like?
- **Incremental value:** Does it add decisions, pitfalls or recovery beyond tool help and shared
  policy, or merely repeat instructions?
- **Overlap:** Does it have a distinct trigger/output? Merge only when workflows and contracts
  coincide; shared verification principles alone do not make skills redundant.
- **Reliability:** Are commands real, prerequisites explicit, permissions preserved and failures
  handled honestly? Can a novice follow it without guessing unsupported APIs?
- **Cost:** Is catalog selection clear? Does the procedure demand unnecessary tools, token use,
  full-suite runs or context reloads?
- **Evidence:** Are there representative successful and failure cases? Are usage and outcomes
  actually measured, or merely assumed?

For measured comparisons, use the same task inputs, model/profile, tools, budgets and environment
with and without the skill. Include positive triggers, neighboring tasks where it should *not*
activate, and unavailable-dependency/failure cases. Record outcome correctness, regressions, tool
calls, latency/token cost and unnecessary actions. Inspect traces, not just load counts: loading a
skill is not evidence it helped. Use consented/redacted data; do not scrape private sessions or
add automatic telemetry for an audit. Unit tests establish loader contracts, not better reasoning.

Keep a skill when it has a distinct useful procedure. Rewrite unsupported or ceremonial guidance;
merge genuine duplication; add a new skill only for an evidenced recurring gap that is not better
served by an existing skill or tool. The current decisions and verification limits are recorded in
[the skills audit](audits/skills-review.md).
