//! Bounded before/after previews. One enclosing hunk retains exact line numbering; no quadratic diff.
const MAX_ROWS: usize = 40;
const MAX_BYTES: usize = 8192;

pub(super) fn preview(before: &str, after: &str) -> String {
    if before == after { return String::new(); }
    let old: Vec<&str> = before.lines().collect();
    let new: Vec<&str> = after.lines().collect();
    let prefix = old.iter().zip(&new).take_while(|(a,b)| a == b).count();
    let suffix = old[prefix..].iter().rev().zip(new[prefix..].iter().rev()).take_while(|(a,b)| a == b).count();
    let removed = old.len() - prefix - suffix;
    let added = new.len() - prefix - suffix;
    let mut out = format!("\nchange preview (+{added} -{removed} lines)\n@@ -{},{} +{},{} @@\n", prefix + 1, removed, prefix + 1, added);
    let mut rows = 0;
    for (sign, lines) in [('-', &old[prefix..old.len()-suffix]), ('+', &new[prefix..new.len()-suffix])] {
        for line in lines {
            if rows == MAX_ROWS || out.len() + line.len() + 2 > MAX_BYTES {
                out.push_str("[diff preview truncated; read the changed region for full content]\n");
                return out;
            }
            out.push(sign); out.push_str(line); out.push('\n'); rows += 1;
        }
    }
    if before.ends_with('\n') != after.ends_with('\n') {
        out.push_str(if after.ends_with('\n') { "[final newline added]\n" } else { "[final newline removed]\n" });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn changing_middle_retains_line_location_and_both_versions() {
        let p = preview("keep\nold\nend\n", "keep\nnew\nend\n");
        assert!(p.contains("@@ -2,1 +2,1 @@\n-old\n+new\n"));
        assert!(!p.contains("-keep"));
    }
    #[test]
    fn bounds_large_replacements_and_reports_newline_changes() {
        assert!(preview(&"old\n".repeat(100), "new\n").contains("truncated"));
        assert!(preview("same", "same\n").contains("final newline added"));
    }
}
