---
name: pack-review
description: Transfer an existing LoRA candidate through verified packs without activating it or carrying exporter evaluation authority.
---

# Portable candidate review

## Outcome
A verified imported LoRA candidate, with the destination's active model unchanged and local evaluation
still required. Use for an existing learned artifact, not for sharing arbitrary skills or config.

## Prerequisites and permissions
Both machines need the same pinned local base checkpoint and a configured Dial state directory.
The exporter needs an existing LoRA version with adapter weights. Obtain permission to read model
artifacts and write the chosen pack and private destination state. No hosted credentials, tokens,
private training samples or whole state directories should be included. Import requires no model
inference; subsequent evaluation/training requires the local runtime prerequisites documented in
`docs/improvement.md`. Never interpret this procedure or pack provenance as execution approval.

## Workflow
1. Identify the exact candidate version and the authorized export path. Export with
   `dial improve export <version> --out <path>`; do not export an unrelated active model by guesswork.
2. Transfer only the resulting pack using a user-approved mechanism. Checksums detect corruption,
   not trusted authorship; inspect provenance and obtain the user's trust decision separately.
3. On the destination with the matching base configured, run `dial improve import <path>`.
   Fingerprint, member, size or digest failure means stop and report the actual error. Do not bypass
   verification, rename internal members or overwrite private state to force acceptance.
4. Record the returned candidate identity. Import does not activate it. A duplicate import is an
   error; inspect the already imported candidate rather than deleting it and retrying blindly.
5. If requested, export the imported candidate again using its new local version ID. Base-path
   localization and provenance can change; adapter weight bytes must remain identical. Local
   evaluation and promotion are separate explicit decisions, not steps implied by importing.

## Verification and report
Report source/destination candidate IDs, observed export/import results, base match and blockers.
Do not claim model quality from archive integrity. Confirm the active version is unchanged before
claiming safe staging. The offline pack roundtrip regression uses synthetic bytes to test this
contract; it is not an inference or quality evaluation.
