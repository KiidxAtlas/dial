//! Prompt text for `/cleanse` discovery and repair workers.

use serde_json::{Value, json};

use super::repair::Assignment;

const DISCOVERY: &str = "<critical>\n\
Never edit project files. Run each proposed command exactly once. Your final output MUST be exactly one JSON object matching the schema, with no prose or code fences.\n\
</critical>\n\
\n\
# Checker Discovery\n\
\n\
User requested `/cleanse` detect and repair: **{{request}}**\n\
\n\
Find the checker commands that produce the diagnostics the user asked for, run them to confirm they work, and report them.\n\
\n\
1. Inspect the project layout, manifests, lockfiles, and configuration to identify languages and tools in use.\n\
2. Propose only commands that produce machine-readable diagnostics for the request. Prefer JSON or unix-style output.\n\
3. Run each command once from its project-relative `cwd`. Discard commands that fail to start or emit nothing parseable.\n\
4. Assign each command the parser id that matches its output format.\n\
5. Return the verified commands as the JSON object.\n\
\n\
## Known parsers\n\
\n\
| id | output |\n\
| --- | --- |\n\
| rust | `cargo ... --message-format=json` compiler messages |\n\
| rust-test | `cargo test --message-format=json` with panics |\n\
| go | `go vet -json` or analyzer JSON |\n\
| go-test | `go test -json` |\n\
| staticcheck | `staticcheck -f json` |\n\
| golangci | `golangci-lint run` text output |\n\
| ruff | `ruff check --output-format=json` |\n\
| pyright | `pyright --outputjson` |\n\
| mypy | `mypy --no-error-summary --no-pretty` (generic text) |\n\
| pylint | `pylint --output-format=json` |\n\
| flake8 | `flake8` default text |\n\
| ty | `ty check --output-format concise` |\n\
| eslint | `eslint --format=json` |\n\
| biome | `biome check --reporter=json` |\n\
| oxlint | `oxlint --format=unix` |\n\
| deno-lint | `deno lint --json` |\n\
| stylelint | `stylelint --formatter json` |\n\
| shellcheck | `shellcheck --format=json1` |\n\
| generic | `file:line:col: severity: message` (GCC or MSVC style) |\n\
\n\
## Output schema\n\
\n\
```json\n\
{\"checkers\":[{\"label\":\"string\",\"cwd\":\"project-relative directory\",\"command\":[\"argv\", \"...\"],\"parser\":\"parser id\"}]}\n\
```\n\
\n\
Notes:\n\
- `command` is an argv array. NEVER wrap it in a shell.\n\
- `cwd` is project-relative. Use `.` for the project root.\n\
- Omit `parser` only when the output is not structured and `generic` does not fit.\n\
- Return `\"checkers\": []` only when nothing in the project produces the requested diagnostics.\n";

const ASSIGNMENT: &str = "<critical>\n\
Fix every assigned diagnostic at its root cause. Stay inside your write scope. NEVER suppress a valid diagnostic. NEVER delegate. NEVER run project-wide checks or formatters.\n\
</critical>\n\
\n\
# Assignment\n\
\n\
Repair worker {{worker}}. Further diagnostics for your files may arrive as chat messages while you work; fix those too before yielding.\n\
\n\
## Write scope\n\
\n\
{{write_scope}}\n\
\n\
Read related code freely. Project-level diagnostics MAY require the smallest necessary edit outside named files; message its owner via `write agent://<id>` before touching a peer-owned file.\n\
\n\
## Diagnostics\n\
\n\
{{diagnostics}}\n\
\n\
## Checker commands\n\
\n\
{{checker_commands}}\n\
\n\
## Concurrent peer ownership\n\
\n\
{{peer_assignments}}\n\
\n\
<workflow>\n\
1. Inspect each diagnostic location.\n\
2. Fix causes, not text or configuration. Do not silence checkers.\n\
3. Preserve behavior unless the diagnostic is a behavioral bug.\n\
4. Run only targeted checks for the files you changed.\n\
5. Re-read your edits and resolve any remaining diagnostic.\n\
</workflow>\n\
\n\
<completeness>\n\
Done means every assigned diagnostic has a fix. If a diagnostic is blocked, name the prerequisite. Your final output lists the changed files and any unresolved items.\n\
</completeness>\n\
\n\
<critical>\n\
You MUST leave peer-owned files untouched and complete the full assignment.\n\
</critical>\n";

pub fn render_discovery(request: &str) -> String {
    DISCOVERY.replace("{{request}}", request)
}

/// JSON schema the scout must satisfy.
pub fn discovery_schema() -> Value {
    json!({
        "type": "object",
        "required": ["checkers"],
        "properties": {
            "checkers": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["label", "command"],
                    "properties": {
                        "label": {"type": "string"},
                        "cwd": {"type": "string"},
                        "command": {"type": "array", "items": {"type": "string"}},
                        "parser": {"type": "string"}
                    }
                }
            }
        }
    })
}

/// Renders the repair assignment for one batch. `checker_commands` lists the suite's commands.
pub fn render_assignment(assignment: &Assignment, checker_commands: &[String]) -> String {
    let scope = if assignment.write_scope.is_empty() {
        "(project-level diagnostics; no named files)".to_owned()
    } else {
        assignment.write_scope.join("\n")
    };
    let peers = if assignment.peers.is_empty() {
        "(no concurrent workers)".to_owned()
    } else {
        assignment.peers.join("\n")
    };
    ASSIGNMENT
        .replace("{{worker}}", &assignment.index.to_string())
        .replace("{{write_scope}}", &scope)
        .replace("{{diagnostics}}", &assignment.diagnostics)
        .replace("{{checker_commands}}", &checker_commands.join("\n"))
        .replace("{{peer_assignments}}", &peers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_prompt_names_the_request() {
        let text = render_discovery("ts errors");
        assert!(text.contains("**ts errors**"));
        assert!(!text.contains("{{request}}"));
    }

    #[test]
    fn assignment_fills_every_placeholder() {
        let assignment = Assignment {
            index: 2,
            write_scope: vec!["a.ts".into()],
            peers: vec![],
            diagnostics: "- [error] a.ts:1 — tsc: boom".into(),
        };
        let text = render_assignment(&assignment, &["tsc --noEmit".into()]);
        assert!(text.contains("Repair worker 2."));
        assert!(text.contains("tsc --noEmit"));
        assert!(!text.contains("{{"));
    }
}
