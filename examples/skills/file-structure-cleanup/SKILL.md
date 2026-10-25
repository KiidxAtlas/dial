---
name: file-structure-cleanup
description: Plan or implement file and directory reorganizations with explicit path mappings, ownership decisions and reference updates.
---

# File structure cleanup

## Outcome
Improve navigation and ownership while preserving public imports, build behavior and runtime discovery.

## Scope
Use for layout, ownership or naming migrations, not dead-code cleanup or behavioral rewrites. Assessment and plan requests are read-only. A small rename needs a concise mapping, not a repository-wide architecture exercise.

## Workflow
1. Establish the package/workspace root, requested scope, relevant worktree changes and existing architecture. Inventory relevant paths with the available file-discovery tool, including hidden configuration when needed. Exclude generated output, dependencies and caches.
2. Identify a concrete navigation or ownership problem. Depth, file counts and names such as utils are signals, not defects. Check framework-required locations, public exports, deployment paths and consumers. Consult version-appropriate documentation only when the decision depends on framework behavior.
3. Map old paths to new paths with the reason, inbound references and compatibility risk. Show a compact before/after tree for broad moves. Trace imports, aliases, barrel exports, dynamic loaders, fixtures, resources, manifests, build scripts, CI and documentation.
4. Preserve public import compatibility where required. Account for case-only renames and case-insensitive filesystem collisions. Keep moves separate from behavior changes and speculative component splitting.
5. When implementation is authorized, migrate one coherent group and its references together. Prefer supported language-aware rename operations and inspect their edits. Keep a recoverable plan for broad migrations in the project's existing planning location; preserve unfinished plans and unrelated changes.

## Verification
Check destinations and remaining old-path references, distinguishing compatibility references from stale ones. Run affected tests and type/build checks. Exercise runtime discovery and packaged assets when involved. Inspect the diff for accidental deletion and resolve failures before the next group. Do not reset unrelated work or commit without authorization.

## Report
For a plan, give mappings, migration order, risks and exact available checks without claiming files moved. For implementation, give completed mappings, checks and outstanding compatibility limits.
