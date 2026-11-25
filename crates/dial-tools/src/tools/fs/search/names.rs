//! Name-only Find: the same scope resolution, ignore rules and traversal as Glob,
//! but literal words instead of glob syntax and no content previews or edit witnesses.
use super::{SearchWalk, Workspace, native_pattern, opt_str, resolve_scope_input};
use serde_json::Value;

pub(in crate::tools) fn find_paths(
    ws: &Workspace,
    args: &Value,
    query: &str,
    limit: u64,
    budget: usize,
) -> Result<String, String> {
    let query = native_pattern(query).to_lowercase();
    let terms: Vec<_> = query.split_whitespace().collect();
    if terms.len() > 32 {
        return Err("path mode accepts at most 32 whitespace-separated words".into());
    }
    let input = opt_str(args, "path")?.unwrap_or(".");
    let scope = resolve_scope_input(ws, input, false)?
        .roots
        .pop()
        .expect("one resolved scope");
    let mut walk = SearchWalk::new(ws, true, true, None);
    let mut text = String::new();
    let mut count = 0;
    let mut non_utf8 = 0;
    let mut clipped = false;
    // Match the same workspace-relative names whether or not a nested root was also granted.
    let root = if scope.starts_with(ws.root()) {
        ws.root()
    } else {
        ws.root_containing(&scope)
            .ok_or("scope is outside workspace roots")?
    };
    walk.run_entries(&scope, root, &mut |path, directory| {
        let Some(name) = path.strip_prefix(root).ok().and_then(|p| p.to_str()) else {
            non_utf8 += 1;
            return true;
        };
        let normalized = native_pattern(name).to_lowercase();
        // Terms may span components, but don't list every descendant of a matching directory.
        let basename = normalized.rfind('/').map_or(0, |slash| slash + 1);
        let mut own_name = false;
        for &term in &terms {
            let Some(start) = normalized.rfind(term) else {
                return true;
            };
            own_name |= start + term.len() > basename;
        }
        if !own_name {
            return true;
        }
        let mut row = ws.rel(path);
        if directory {
            row.push('/');
        }
        if row.contains(char::is_control) {
            row = serde_json::json!(row).to_string();
        }
        row.push('\n');
        // Reserve room for truthful coverage even on a page full of long names.
        if count == limit || text.len() + row.len() > budget * 3 / 4 {
            clipped = true;
            return false;
        }
        count += 1;
        text.push_str(&row);
        true
    });
    if count == 0 && !clipped {
        text.push_str("no matching names in searched paths\n");
    }
    if clipped || walk.next.is_some() {
        text.push_str("[path results limited; narrow query or scope]\n");
    }
    let mut footer = format!("[path names only; results={count}; {}", walk.coverage());
    if non_utf8 > 0 {
        footer.push_str(&format!("; non_utf8_paths_excluded={non_utf8}"));
    }
    footer.push_str("]; ignored/policy subtrees not searched. For an intended ignored directory, set path explicitly (e.g. path=\".tmp\"); access restrictions still apply.\n");
    if text.len() + footer.len() > budget {
        const CLIPPED: &str = " …[coverage details clipped]\n";
        footer.truncate(footer.floor_char_boundary(budget - text.len() - CLIPPED.len()));
        footer.push_str(CLIPPED);
    }
    text.push_str(&footer);
    Ok(text)
}
