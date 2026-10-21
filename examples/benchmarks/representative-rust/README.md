# Non-default Rust workspace member

No external dependencies. `cargo test --offline` selects housekeeping, which has a known unrelated baseline failure. Pricing work belongs to the non-default `pricing` member and its `checkout` consumer. Preserve housekeeping, the workspace defaults and public signatures. Use package-scoped tests rather than repairing the unrelated baseline. The independent acceptance harness is supplied by the suite, not by mutable fixture tests.
