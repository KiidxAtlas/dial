# Curated workflows using existing Dial formats

These are ordinary on-demand skills, not executable recipes, configuration overlays or a second
interpreter. They are inactive until imported. Read their prerequisites and trust boundaries first.

| Skill | Composition | Permissions and acceptance |
| --- | --- | --- |
| `review` | Read-only review + existing session export | Project reads; checks/export only with their own permission. Findings cite evidence; report export is not session replay. |
| `pack-review` | Existing LoRA candidate + verified pack export/import/re-export | Artifact reads, chosen output and private-state writes. Matching pinned base; imports remain candidates, active version unchanged, local evaluation required. |

From this checkout, preview then import into the project where you intend to use them:

```sh
dial skills import ./examples/workflows --dry-run
dial skills import ./examples/workflows
dial skills list
```

From another project, use the absolute source directory instead. Invoke `/skill:review` or
`/skill:pack-review` in chat after checking the effective catalog. For same-source updates, preview
with `--update --dry-run`, then use `--update`; local modifications are protected, not overwritten.
Do not use `--user` unless you want the procedures available across projects.

Portability has two distinct contracts: skill directories are copied as Markdown and optional assets;
`.dialpack` archives carry **only learned LoRA/steering artifacts**, never a workflow interpreter,
credentials or tool grants. There is no skill `export` command: sharing the authored directory is the
portable source format. The pack workflow uses the real `dial improve export` / `import` commands.

Tests use temporary directories and synthetic model/adapter bytes, no network, credentials, model
loading or evaluation. `pack/tests.rs::workflow_adapter_export_import_export_roundtrip_keeps_weights_and_local_gate`
exercises both public export and import APIs and a second export, checks archive contents and verifies
that staging never activates the candidate. Skill loader/import tests validate the actual Markdown
fixtures. These establish format and trust contracts, not improved model task success.

See [skills](../../docs/skills.md), [portable learned packs](../../docs/improvement.md) and
[client contracts](../../docs/client-contracts.md) for the underlying owners and optional runtime
prerequisites. Never put secrets or private session content into these maintained examples.
