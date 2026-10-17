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

pub(crate) fn rule(pattern: &str, tool: &str, message: &str) -> Rule {
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
    ]
}
