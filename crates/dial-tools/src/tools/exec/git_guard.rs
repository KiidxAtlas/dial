//! Git guard: refuses shell commands that discard uncommitted work in a shared checkout.
//!
//! Agents that share one checkout lose each other's edits when one of them runs `git reset --hard`,
//! `git stash`, `git checkout -- <path>`, `git restore <path>`, `git clean -f` or
//! `git switch --discard-changes`. `shell.git_guard` decides where the guard applies (see
//! [`Workspace::git_guard_active`]); this module only judges a command.
//!
//! The command is read before it runs, so only invocations that are visible in the script are
//! judged: `git` reached through a variable, an alias, a script file or `xargs` is not. Scripts the
//! lexer cannot read (an unterminated quote) are never refused on a guess, and an argument that
//! needs shell expansion (`$VAR`, `$(...)`) is not treated as a match. `-C <dir>`, a preceding
//! `cd`, `env`/`command`/`exec` wrappers, `sh -c '...'` scripts, subshells and `if`/`then` bodies
//! are followed. A git command whose resolved directory lies outside every workspace root works on
//! another repository and is allowed; an unknown directory counts as inside.

use crate::tools::surface::outcome::Effects;
use crate::tools::{ToolOutput, ToolStatus, Workspace};
use std::path::{Component, Path, PathBuf};

/// The refusal for `command` run in `cwd`, when the workspace's guard is active and the command
/// discards uncommitted work.
pub(super) fn guard(ws: &Workspace, command: &str, cwd: &Path) -> Option<ToolOutput> {
    if !ws.git_guard_active() {
        return None;
    }
    let roots: Vec<&Path> = ws.roots().collect();
    let danger = find(command, cwd, &roots)?;
    let always = ws.git_guard_policy() == crate::config::types::GitGuard::Always;
    Some(
        ToolOutput::err(danger.message(always))
            .with_status(ToolStatus::Blocked)
            .with_effects(Effects::None),
    )
}

/// What a refused command would have done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Danger {
    /// The offending invocation, such as `git reset --hard`.
    pub form: String,
    /// What it destroys.
    effect: &'static str,
    /// Safe forms that reach the same goal.
    instead: &'static str,
}

impl Danger {
    fn message(&self, always: bool) -> String {
        let scope = if always {
            "this workspace (shell.git_guard = \"always\")"
        } else {
            "a checkout shared with the parent agent (shell.git_guard = \"workers\")"
        };
        format!(
            "`{}` was not run: it {} in {scope}, where other edits are uncommitted and not yours to discard.\n\
             Instead: {}.\n\
             If the discard is really intended, ask the user to run it or to lift the guard \
             (`shell.git_guard = \"off\"`); a worker that must reset or stash freely should run \
             isolated (`isolation: \"copy\"`).",
            self.form, self.effect, self.instead
        )
    }
}

const DISCARD_TREE: &str = "overwrites or deletes working-tree files and unstaged changes";
const UNSTAGE: &str = "`git restore --staged <path>` or `git reset <path>` unstage without touching files; `git diff`/`git show` inspect; commit or `git checkout -b <branch>` keeps work";

/// The first destructive git invocation in `command`, judged from `cwd` and the workspace `roots`.
pub(super) fn find(command: &str, cwd: &Path, roots: &[&Path]) -> Option<Danger> {
    scan_script(command, &Dir::Known(cwd.to_path_buf()), roots, 0)
}

// ------------------------------------------------------------------------------------- lexing

#[derive(Debug, Clone)]
struct Word {
    text: String,
    /// Needs shell expansion (`$x`, `$(..)`, backticks) so its value is unknown here.
    dynamic: bool,
}

#[derive(Debug)]
enum Token {
    Word(Word),
    /// A command boundary; `true` when the command's output is piped on (its `cd` has no effect).
    Sep(bool),
    Open,
    Close,
}

/// Splits `script` into words and command boundaries. `None` for an unterminated quote.
fn lex(script: &str) -> Option<Vec<Token>> {
    let chars: Vec<char> = script.chars().collect();
    let mut tokens = Vec::new();
    let mut word = String::new();
    let mut dynamic = false;
    let mut active = false;
    let mut heredocs: Vec<(String, bool)> = Vec::new();
    let mut i = 0;
    macro_rules! flush {
        () => {
            if active {
                tokens.push(Token::Word(Word {
                    text: std::mem::take(&mut word),
                    dynamic: std::mem::take(&mut dynamic),
                }));
                active = false;
            }
        };
    }
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' => {
                active = true;
                i += 1;
                while chars.get(i).is_some_and(|&c| c != '\'') {
                    word.push(chars[i]);
                    i += 1;
                }
                chars.get(i)?;
            }
            '"' => {
                active = true;
                i += 1;
                loop {
                    match *chars.get(i)? {
                        '"' => break,
                        '\\' => {
                            i += 1;
                            let escaped = *chars.get(i)?;
                            if !matches!(escaped, '$' | '`' | '"' | '\\' | '\n') {
                                word.push('\\');
                            }
                            if escaped != '\n' {
                                word.push(escaped);
                            }
                        }
                        '$' | '`' => {
                            dynamic = true;
                            word.push(chars[i]);
                        }
                        other => word.push(other),
                    }
                    i += 1;
                }
            }
            '\\' => {
                active = true;
                i += 1;
                match chars.get(i) {
                    Some('\n') | None => {}
                    Some(&escaped) => word.push(escaped),
                }
            }
            '$' => {
                active = true;
                dynamic = true;
                word.push('$');
            }
            '#' if !active => {
                while chars.get(i + 1).is_some_and(|&c| c != '\n') {
                    i += 1;
                }
            }
            '<' if chars.get(i + 1) == Some(&'<') && chars.get(i + 2) != Some(&'<') => {
                flush!();
                i += 2;
                let strip_tabs = chars.get(i) == Some(&'-');
                if strip_tabs {
                    i += 1;
                }
                while chars.get(i).is_some_and(|c| matches!(c, ' ' | '\t')) {
                    i += 1;
                }
                let mut delimiter = String::new();
                while let Some(&next) = chars.get(i) {
                    match next {
                        ' ' | '\t' | '\n' | ';' | '&' | '|' | '(' | ')' | '<' | '>' => break,
                        '\'' | '"' | '\\' => {}
                        other => delimiter.push(other),
                    }
                    i += 1;
                }
                heredocs.push((delimiter, strip_tabs));
                continue;
            }
            '\n' => {
                flush!();
                tokens.push(Token::Sep(false));
                i += 1;
                for (delimiter, strip_tabs) in heredocs.drain(..) {
                    while i < chars.len() {
                        let end = chars[i..]
                            .iter()
                            .position(|&c| c == '\n')
                            .map_or(chars.len(), |p| i + p);
                        let line: String = chars[i..end].iter().collect();
                        i = (end + 1).min(chars.len());
                        let line = if strip_tabs {
                            line.trim_start_matches('\t')
                        } else {
                            &line
                        };
                        if line == delimiter {
                            break;
                        }
                    }
                }
                continue;
            }
            ' ' | '\t' | '\r' => flush!(),
            ';' => {
                flush!();
                tokens.push(Token::Sep(false));
            }
            '&' if matches!(chars.get(i.wrapping_sub(1)), Some('>' | '<'))
                || chars.get(i + 1) == Some(&'>') =>
            {
                active = true;
                word.push('&');
            }
            '&' | '|' => {
                flush!();
                let piped = c == '|' && chars.get(i + 1) != Some(&'|');
                if matches!(chars.get(i + 1), Some(&next) if next == c || (c == '|' && next == '&'))
                {
                    i += 1;
                }
                tokens.push(Token::Sep(piped));
            }
            '(' => {
                flush!();
                tokens.push(Token::Open);
            }
            ')' => {
                flush!();
                tokens.push(Token::Close);
            }
            '`' => {
                flush!();
                tokens.push(Token::Sep(false));
            }
            other => {
                active = true;
                word.push(other);
            }
        }
        i += 1;
    }
    if active {
        tokens.push(Token::Word(Word {
            text: word,
            dynamic,
        }));
    }
    Some(tokens)
}

// -------------------------------------------------------------------------------- directories

/// Where a command runs: a resolved path, or unknown after an expansion.
#[derive(Debug, Clone)]
enum Dir {
    Known(PathBuf),
    Unknown,
}

impl Dir {
    fn join(&self, target: &Word) -> Dir {
        match self {
            Dir::Known(base) if !target.dynamic && !target.text.starts_with('~') => {
                Dir::Known(settle(&base.join(&target.text)))
            }
            _ => Dir::Unknown,
        }
    }

    /// Whether git here works on the guarded checkout. Unknown counts as inside.
    fn inside(&self, roots: &[&Path]) -> bool {
        match self {
            Dir::Known(path) => roots.iter().any(|root| path.starts_with(root)),
            Dir::Unknown => true,
        }
    }
}

/// `path` with `.`/`..` folded, canonical when it exists.
fn settle(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return real;
    }
    let mut folded = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                folded.pop();
            }
            Component::CurDir => {}
            other => folded.push(other),
        }
    }
    folded
}

// ------------------------------------------------------------------------------------ walking

const MAX_SCRIPT_DEPTH: usize = 3;

fn scan_script(script: &str, start: &Dir, roots: &[&Path], depth: usize) -> Option<Danger> {
    let tokens = lex(script)?;
    let mut dir = start.clone();
    let mut saved: Vec<Dir> = Vec::new();
    let mut words: Vec<Word> = Vec::new();
    for token in tokens {
        let piped = match token {
            Token::Word(word) => {
                words.push(word);
                continue;
            }
            Token::Sep(piped) => piped,
            Token::Open => {
                let found = command(&std::mem::take(&mut words), &mut dir, false, roots, depth);
                saved.push(dir.clone());
                if found.is_some() {
                    return found;
                }
                continue;
            }
            Token::Close => {
                let found = command(&std::mem::take(&mut words), &mut dir, false, roots, depth);
                if found.is_some() {
                    return found;
                }
                if let Some(outer) = saved.pop() {
                    dir = outer;
                }
                continue;
            }
        };
        if let found @ Some(_) = command(&std::mem::take(&mut words), &mut dir, piped, roots, depth)
        {
            return found;
        }
    }
    command(&words, &mut dir, false, roots, depth)
}

/// Keywords that may precede a command in the same list.
const KEYWORDS: &[&str] = &[
    "if", "then", "else", "elif", "do", "while", "until", "!", "{", "}", "time",
];

/// One simple command: applies `cd`, follows wrappers and `sh -c`, judges `git`.
fn command(
    words: &[Word],
    dir: &mut Dir,
    piped: bool,
    roots: &[&Path],
    depth: usize,
) -> Option<Danger> {
    let mut words = words;
    loop {
        let first = words.first()?;
        let name = first.text.as_str();
        if KEYWORDS.contains(&name)
            || matches!(name, "command" | "exec" | "nohup" | "builtin")
            || (is_assignment(name) && !name.starts_with('='))
        {
            words = &words[1..];
        } else if name == "env" {
            // Options and VAR=value pairs; `-u NAME` and `-C DIR` take a value.
            let mut rest = &words[1..];
            while let Some(next) = rest.first() {
                let skip = if matches!(next.text.as_str(), "-u" | "-C" | "-S") {
                    2
                } else if next.text.starts_with('-') || is_assignment(&next.text) {
                    1
                } else {
                    break;
                };
                rest = &rest[skip.min(rest.len())..];
            }
            words = rest;
        } else {
            break;
        }
    }
    let first = words.first()?;
    if first.dynamic {
        return None;
    }
    let program = first.text.rsplit('/').next().unwrap_or(&first.text);
    match program {
        "cd" | "pushd" => {
            if !piped {
                let target = words[1..]
                    .iter()
                    .find(|word| !matches!(word.text.as_str(), "--" | "-P" | "-L" | "-e" | "-@"));
                *dir = match target {
                    Some(target) if target.text != "-" => dir.join(target),
                    _ => Dir::Unknown,
                };
            }
            None
        }
        "popd" => {
            if !piped {
                *dir = Dir::Unknown;
            }
            None
        }
        "sh" | "bash" | "zsh" | "dash" | "ksh" if depth < MAX_SCRIPT_DEPTH => {
            let script = words.iter().enumerate().skip(1).find_map(|(index, word)| {
                let flags = word.text.strip_prefix('-')?;
                (!flags.starts_with('-') && flags.contains('c'))
                    .then(|| words.get(index + 1))
                    .flatten()
            })?;
            if script.dynamic {
                return None;
            }
            scan_script(&script.text, dir, roots, depth + 1)
        }
        "git" => git(&words[1..], dir, roots),
        _ => None,
    }
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !name.starts_with(|c: char| c.is_ascii_digit())
    })
}

// ----------------------------------------------------------------------------------------- git

fn git(args: &[Word], dir: &Dir, roots: &[&Path]) -> Option<Danger> {
    let mut dir = dir.clone();
    let mut at = 0;
    let sub = loop {
        let word = args.get(at)?;
        at += 1;
        let text = word.text.as_str();
        match text {
            "-C" => dir = dir.join(args.get(at)?),
            "-c" | "--config-env" | "--namespace" | "--super-prefix" | "--attr-source"
            | "--exec-path" => {}
            "--git-dir" | "--work-tree" => dir = Dir::Unknown,
            _ if text.starts_with("--git-dir=") || text.starts_with("--work-tree=") => {
                dir = Dir::Unknown;
                at -= 1;
            }
            _ if text.starts_with('-') => at -= 1,
            _ => break word,
        }
        at += 1;
    };
    if sub.dynamic {
        return None;
    }
    let sub = Sub::parse(&sub.text)?;
    let args = &args[at..];
    let danger = classify(sub, args, &dir)?;
    dir.inside(roots).then_some(danger)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sub {
    Stash,
    Reset,
    Checkout,
    Switch,
    Restore,
    Clean,
}

impl Sub {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "stash" => Self::Stash,
            "reset" => Self::Reset,
            "checkout" => Self::Checkout,
            "switch" => Self::Switch,
            "restore" => Self::Restore,
            "clean" => Self::Clean,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::Stash => "stash",
            Self::Reset => "reset",
            Self::Checkout => "checkout",
            Self::Switch => "switch",
            Self::Restore => "restore",
            Self::Clean => "clean",
        }
    }

    /// Short and long options whose value may be the next argument.
    fn valued(self) -> (&'static [char], &'static [&'static str]) {
        match self {
            Self::Stash => (&[], &[]),
            Self::Reset => (
                &['U'],
                &["pathspec-from-file", "unified", "inter-hunk-context"],
            ),
            Self::Checkout => (
                &['b', 'B', 'U'],
                &[
                    "orphan",
                    "conflict",
                    "pathspec-from-file",
                    "unified",
                    "inter-hunk-context",
                ],
            ),
            Self::Switch => (&['c', 'C'], &["orphan", "conflict"]),
            Self::Restore => (
                &['s', 'U'],
                &[
                    "source",
                    "conflict",
                    "pathspec-from-file",
                    "unified",
                    "inter-hunk-context",
                ],
            ),
            Self::Clean => (&['e'], &["exclude"]),
        }
    }
}

/// One subcommand argument, read the way git's option parser reads it.
#[derive(Debug, Clone)]
enum Arg {
    /// `--name` or `--name=value`, by name.
    Long(String),
    /// One letter of a short-option bundle: `-qf` yields `q`, then `f`.
    Short(char),
    /// A non-option argument before `--`.
    Operand(Word),
    /// An argument after `--`.
    Pathspec(Word),
}

fn scan(sub: Sub, args: &[Word]) -> Vec<Arg> {
    let (short_valued, long_valued) = sub.valued();
    let mut scanned = Vec::new();
    let mut options = true;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let text = arg.text.as_str();
        if text == "--" && options {
            scanned.extend(args.by_ref().cloned().map(Arg::Pathspec));
            break;
        }
        if !options {
            scanned.push(Arg::Operand(arg.clone()));
        } else if text == "--end-of-options" {
            options = false;
        } else if let Some(long) = text.strip_prefix("--") {
            let (name, inline) = long
                .split_once('=')
                .map_or((long, false), |(name, _)| (name, true));
            if !inline && long_valued.contains(&name) {
                args.next();
            }
            scanned.push(Arg::Long(name.to_owned()));
        } else if let Some(bundle) = text.strip_prefix('-').filter(|bundle| !bundle.is_empty()) {
            for (at, flag) in bundle.char_indices() {
                scanned.push(Arg::Short(flag));
                if short_valued.contains(&flag) {
                    // The value is the rest of the bundle, else the next argument.
                    if at + flag.len_utf8() == bundle.len() {
                        args.next();
                    }
                    break;
                }
            }
        } else {
            scanned.push(Arg::Operand(arg.clone()));
        }
    }
    scanned
}

/// Whether option `name` spells `full` (git accepts any unambiguous prefix of two or more letters).
fn is_long(name: &str, full: &str) -> bool {
    name.len() >= 2 && full.starts_with(name)
}

fn has_long(args: &[Arg], full: &str) -> bool {
    args.iter()
        .any(|arg| matches!(arg, Arg::Long(name) if is_long(name, full)))
}

fn has_short(args: &[Arg], flag: char) -> bool {
    args.iter()
        .any(|arg| matches!(arg, Arg::Short(f) if *f == flag))
}

fn classify(sub: Sub, args: &[Word], dir: &Dir) -> Option<Danger> {
    let scanned = scan(sub, args);
    let form = |detail: &str| {
        if detail.is_empty() {
            format!("git {}", sub.name())
        } else {
            format!("git {} {detail}", sub.name())
        }
    };
    match sub {
        Sub::Stash => {
            let first = scanned.iter().find_map(|arg| match arg {
                Arg::Operand(word) => Some(word.text.as_str()),
                _ => None,
            });
            if matches!(first, Some("list" | "show" | "create" | "store")) {
                return None;
            }
            Some(Danger {
                form: form(first.unwrap_or("")),
                effect: "moves every uncommitted change out of the working tree and index",
                instead: "`git stash list`/`git stash show` read stashes; to set work aside, commit it on a branch (`git checkout -b <branch>`)",
            })
        }
        Sub::Reset => has_long(&scanned, "hard").then(|| Danger {
            form: form("--hard"),
            effect: "resets the working tree and index to a commit, discarding uncommitted changes",
            instead: "`git reset --soft`/`--mixed`, `git reset <path>` and `git restore --staged <path>` keep file contents",
        }),
        Sub::Switch => {
            let forced = has_short(&scanned, 'f')
                || has_long(&scanned, "force")
                || has_long(&scanned, "discard-changes");
            forced.then(|| Danger {
                form: form("--discard-changes"),
                effect: DISCARD_TREE,
                instead: "plain `git switch <branch>` refuses instead of discarding; commit first",
            })
        }
        Sub::Clean => {
            let force = has_short(&scanned, 'f') || has_long(&scanned, "force");
            let dry = has_short(&scanned, 'n') || has_long(&scanned, "dry-run");
            (force && !dry).then(|| Danger {
                form: form("-f"),
                effect: "deletes untracked files from disk",
                instead: "`git clean -n` lists what it would delete; remove specific files you created with `rm <path>`",
            })
        }
        Sub::Restore => {
            let staged = has_short(&scanned, 'S') || has_long(&scanned, "staged");
            let worktree = has_short(&scanned, 'W') || has_long(&scanned, "worktree");
            (!(staged && !worktree)).then(|| Danger {
                form: form("<path>"),
                effect: DISCARD_TREE,
                instead: UNSTAGE,
            })
        }
        Sub::Checkout => {
            let resolving = has_long(&scanned, "ours") || has_long(&scanned, "theirs");
            let forced = has_short(&scanned, 'f') || has_long(&scanned, "force");
            let creating = has_short(&scanned, 'b')
                || has_short(&scanned, 'B')
                || has_long(&scanned, "orphan")
                || has_long(&scanned, "detach");
            let whole_tree = scanned.iter().any(|arg| match arg {
                Arg::Operand(word) | Arg::Pathspec(word) => is_whole_tree(&word.text),
                _ => false,
            });
            let pathspec = scanned
                .iter()
                .any(|arg| matches!(arg, Arg::Pathspec(_)))
                || has_long(&scanned, "pathspec-from-file");
            // `git checkout <name>` without `--` is a path when a file of that name exists.
            let operands: Vec<&Word> = scanned
                .iter()
                .filter_map(|arg| match arg {
                    Arg::Operand(word) => Some(word),
                    _ => None,
                })
                .collect();
            let names_a_path = !creating
                && matches!(&operands[..], [word] if !word.dynamic && match dir {
                    Dir::Known(base) => base.join(&word.text).exists(),
                    Dir::Unknown => false,
                });
            let destructive = forced || whole_tree || (pathspec && !resolving) || names_a_path;
            destructive.then(|| Danger {
                form: if forced {
                    form("-f")
                } else {
                    form("-- <path>")
                },
                effect: DISCARD_TREE,
                instead: "`git checkout -b <branch>` or `git switch <branch>` keep local changes; `git checkout --ours|--theirs -- <path>` resolves a conflict; `git diff <path>` shows what would be lost",
            })
        }
    }
}

/// Whether `spec` covers everything under the working directory or the repository root: `.`, `*`,
/// `:/`, or a path climbing out with `..`.
fn is_whole_tree(spec: &str) -> bool {
    let rest = spec
        .strip_prefix(":/")
        .or_else(|| spec.strip_prefix(":(top)"))
        .unwrap_or(spec);
    rest == "*" || rest.split('/').all(|part| matches!(part, "" | "." | ".."))
}

#[cfg(test)]
mod tests;
