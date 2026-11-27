//! `[bash_interceptor]` rules that point duplicate shell commands at dedicated tools.

use serde::{Deserialize, Serialize};

/// One bash-interceptor rule: commands matching `pattern` are redirected to `tool`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Rule {
    pub pattern: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flags: Option<String>,
    pub tool: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_subcommands: Option<Vec<String>>,
}

/// What a matching command gets.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum InterceptMode {
    /// Run the command and append a one-line pointer to the dedicated tool.
    #[default]
    Hint,
    /// Refuse the command with the rule's message; the model must reissue it.
    Block,
}

/// `[bash_interceptor]` configuration. `patterns: None` selects [`default_rules`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct BashInterceptorConfig {
    pub enabled: bool,
    pub mode: InterceptMode,
    pub patterns: Option<Vec<Rule>>,
}

impl Default for BashInterceptorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: InterceptMode::Hint,
            patterns: None,
        }
    }
}

pub fn rule(pattern: &str, tool: &str, message: &str) -> Rule {
    Rule {
        pattern: pattern.to_owned(),
        flags: None,
        tool: tool.to_owned(),
        message: message.to_owned(),
        allow_subcommands: None,
    }
}

/// Built-in interception rules, in priority order (first match wins).
pub fn default_rules() -> Vec<Rule> {
    vec![
        // `cat >file` / `cat <<EOF` write rather than read and fall through to the write rule;
        // `tail -f` follows a live file, which `read` cannot, and falls through to the bg rule.
        rule(
            r"^\s*(?:cat(?!\s*(?:>|<<))|head|tail(?!\s+-[fF](?:\s|$))|less|more)\s+",
            "read",
            "Use the `read` tool instead of cat/head/tail. It provides better context and handles binary files.",
        ),
        // Only a bare line-range print with a file operand; `sed -n '/re/p'`, multi-command
        // scripts and stdin filters are left alone.
        rule(
            r#"^\s*sed\s+-n\s+(?:'\d+(?:,\d+)?p'|"\d+(?:,\d+)?p"|\d+(?:,\d+)?p)\s+[^\s|;&<>]+\s*$"#,
            "read",
            "Use the `read` tool with offset/limit instead of sed -n 'A,Bp'.",
        ),
        // Read-like sqlite3 opens only (a bare database, or a SELECT/PRAGMA/dot-command). A
        // writable open can checkpoint and delete a `-wal` file; `-readonly` opens are left alone.
        Rule {
            flags: Some("i".to_owned()),
            ..rule(
                r#"^\s*sqlite3(?!.*\s-readonly\b)\s+(?:-\S+\s+)*[^\s-]\S*(?:\s*$|\s+["']?\s*(?:select|pragma|\.(?:tables|schema|dump|indexes)))"#,
                "read",
                "Use the `read` tool to inspect a database without changing it (`:table` pages rows). A writable sqlite3 open can merge and delete its -wal file; use `sqlite3 -readonly` for queries.",
            )
        },
        // `xxd -r` writes bytes back and keeps the shell.
        rule(
            r"^\s*(?:xxd(?!.*\s-r\b)|hexdump|od)\s+",
            "read",
            "Use the `read` tool with `:bytes=O+N` for a hex view instead of xxd/hexdump/od.",
        ),
        rule(
            r"^\s*(grep|rg|ripgrep|ag|ack)\s+",
            "grep",
            "Use the `grep` tool instead of grep/rg. It respects .gitignore and provides structured output.",
        ),
        // Predicates and actions a glob cannot express (`-exec`, `-delete`, times, sizes,
        // permissions, custom output) keep `find`.
        rule(
            r"^\s*(?!.*\s-(?:exec|execdir|ok|okdir|delete|newer|[acm]min|[acm]time|size|perm|user|group|links|empty|printf|fprint|print0)\b)(find|fd|locate)\s+.*(-name|-iname|-type|--type|-glob)",
            "glob",
            "Use the `glob` tool instead of find/fd. It respects .gitignore and is faster for glob patterns.",
        ),
        rule(
            r"^\s*sed\s+(-i|--in-place)",
            "edit",
            "Use the `edit` tool instead of sed -i. It provides diff preview, and /undo can restore it.",
        ),
        // `-i` must be one of perl's leading switches; an `-i` inside the program text is not.
        rule(
            r"^\s*perl(?:\s+-[A-Za-z]+)*?\s+-[A-Za-z]*i[A-Za-z.]*(?:\s|$)",
            "edit",
            "Use the `edit` tool instead of perl -i. It provides diff preview, and /undo can restore it.",
        ),
        rule(
            r"^\s*awk\s+.*-i\s+inplace",
            "edit",
            "Use the `edit` tool instead of awk -i inplace. It provides diff preview, and /undo can restore it.",
        ),
        // `>` must sit outside quoted regions (so `echo "a -> b"` passes) and be followed by a
        // plausible filename, including `$VAR` targets; `>|` (clobber) counts as a redirect;
        // `>&2`/`2>&1` style fd duplication is not matched. Allowed device sinks are consumed
        // while looking for later real file redirects because the write tool cannot replace
        // shell output/discard targets.
        rule(
            r#"^\s*(echo|printf|cat\s*<<)\s+(?:(?:[^"'>]|"[^"]*"|'[^']*')|(?<!\|)>{1,2}\|?\s*(?:"/dev/(?:null|tty|stdout|stderr)"|'/dev/(?:null|tty|stdout|stderr)'|/dev/(?:null|tty|stdout|stderr))(?:[\s;&|]|$))*(?<!\|)>{1,2}\|?\s*(?!(?:"/dev/(?:null|tty|stdout|stderr)"|'/dev/(?:null|tty|stdout|stderr)'|/dev/(?:null|tty|stdout|stderr))(?:[\s;&|]|$))[$\w./~"'-]"#,
            "write",
            "Use the `write` tool instead of echo/cat redirection. It handles encoding and provides confirmation.",
        ),
        rule(
            r"^\s*nohup\s+|(?<!&)&\s*$",
            "bg",
            "Use `bg` with `action: start` and `name` instead of nohup or a trailing `&`, so the process stays observable and managed.",
        ),
        rule(
            r"^\s*(?:(?:bun|npm|pnpm|yarn)\s+(?:run\s+)?(?:dev|start)(?:\s|$)|(?:vite|next\s+dev|nuxt\s+dev|nodemon|lldb|gdb|tail\s+-[fF])(?:\s|$)|docker\s+compose\s+up(?!.*(?:\s-d(?:\s|$)|--detach))(?:\s|$))",
            "bg",
            "Use `bg` with `action: start` and `name` for services, watchers, and debuggers; inspect with `bg` action `output` or `wait`.",
        ),
        rule(
            r"^\s*(?:(?:(?:bun|npm|pnpm|yarn)\s+(?:run\s+)?\S+|jest)(?:.|\n)*--watch|(?:cargo\s+watch|watchexec|pytest|vitest|tsc)(?:.|\n)*(?:--watch|-w))(?:\s|$)",
            "bg",
            "Use `bg` with `action: start` and `name` for watch mode so its output, input, and lifecycle stay managed.",
        ),
        // Read-only Git/GitHub commands (never `git grep`, mutating commands, or arbitrary flags).
        rule(
            r"^\s*(?:git(?:\s+-C\s+\S+|\s+--no-pager)*\s+(?:status|diff|log|show|blame|branch|reflog|stash\s+(?:list|show)|tag(?:\s+(?:-l|--list)\b|$)|remote(?:\s+(?:-v|--verbose)\b|$)|rev-parse|merge-base)|gh\s+(?:pr\s+(?:view|checks)|issue\s+view))(?:\s|$)",
            "git",
            "Use the `git` tool instead of shell git/gh read commands (pass `cwd` for sub-repositories). It runs unapproved with structured output.",
        ),
        // Live host state inspection belongs in `read` via `sys://` URIs.
        rule(
            r"^\s*(?:ps|lsof|ss|netstat)(?:\s|$)",
            "read",
            "Use the `read` tool with `sys://procs`, `sys://ports`, or `sys://host` for live host state instead of ps/lsof/ss/netstat.",
        ),
        // Simple web-page GETs; downloads, custom methods/headers/bodies, redirects and loopback APIs keep bash.
        rule(
            r"^\s*(?:curl|wget)(?!.*(?:\s-(?:[XdoOFHuT]|-[a-z]*data|-[a-z]*output|-[a-z]*request|-[a-z]*header|-[a-z]*form|-[a-z]*user|-[a-z]*head)\b|\s-I(?:\s|$)|>|localhost|127\.0\.0\.1|\[::1\]))\s+.*https?://",
            "read",
            "Use the `read` tool with an `http(s)://` URL (`fetch: true`) instead of curl/wget for reading web pages.",
        ),
        // Archive directory listings belong in `read` (which decodes archives and `:member` views).
        rule(
            r"^\s*(?:tar\s+-[a-zA-Z]*t[a-zA-Z]*f?\s+|unzip\s+-l\s+)",
            "read",
            "Use the `read` tool to inspect archive listings and members (`:member`) instead of tar -t / unzip -l.",
        ),
        // Directory trees belong in `read` (directory view with pagination) or `glob`.
        rule(
            r"^\s*tree(?:\s|$)",
            "glob",
            "Use `read` on a directory (with optional `offset`/`limit`) or `glob` (`type: \"dir\"|\"all\"`) instead of `tree`.",
        ),
        // Standalone file line counts belong in `read` or `grep` (`output: "count"`).
        rule(
            r"^\s*wc\s+-[a-zA-Z]*l(?:\s|$)",
            "read",
            "Use `read` (which shows line totals) or `grep` (`output: \"count\"`) instead of `wc -l`.",
        ),
        // Binary/file-type sniffing belongs in `read` (which decodes executables, archives, and `:bytes=`).
        rule(
            r"^\s*(?:file|strings)\s+",
            "read",
            "Use the `read` tool instead of `file`/`strings`; it sniffs binary/executable formats and supports `:bytes=O+N`.",
        ),
        // Direct test/lint/fmt/typecheck invocations that `verify` covers natively (placed after watch-mode `bg` rule).
        rule(
            r"^\s*(?:(?:python3?\s+-m\s+)?(?:pytest|unittest|mypy)|pyright|ruff\s+(?:check|format\s+--check)|(?:npx\s+|pnpm\s+|yarn\s+|bunx\s+)?(?:jest|vitest)|node\s+--test|go\s+(?:test|vet|build)|cargo\s+(?:test|check|build|clippy|fmt(?:\s+--all)?\s+--\s+--check|fmt\s+--check))(?:\s|$)",
            "verify",
            "Use the `verify` tool for tests and project checks (`targets: [\"path/to/test\"]` or `[\"dir:clippy|fmt|lint|typecheck|build|test\"]`) so passing results count toward final verification.",
        ),
    ]
}
