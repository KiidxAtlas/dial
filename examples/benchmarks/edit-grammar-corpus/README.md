# Edit-grammar benchmark corpus

Small committed source files for `dial edit-bench` (see `docs/benchmarks.md`). The benchmark derives its
mutation cases from these files, so keep them deterministic: free functions at column 0 whose names
appear only as plain identifiers (never in strings, comments, methods or types), and some deliberately
repeated lines so edits near duplicates are exercised. Changing a file changes every generated case.
