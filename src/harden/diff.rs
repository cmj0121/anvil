//! A line diff rendered as a unified diff with three lines of context.

const CONTEXT: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Equal,
    Delete,
    Insert,
}

/// One line of the edit script: its kind and the line's index in the old
/// and new text (for an insert, `old` is where it lands; for a delete, `new`).
#[derive(Debug, Clone, Copy)]
struct Op {
    kind: Kind,
    old: usize,
    new: usize,
}

/// `old` → `new` as a unified diff headed `--- a/<shown>` / `+++ b/<shown>`
/// (no `a/` / `b/` for an absolute `shown`; `--- /dev/null` when `old` is
/// `None`); empty when nothing changed.
pub fn unified(shown: &str, old: Option<&str>, new: &str) -> String {
    if old == Some(new) {
        return String::new();
    }
    let a: Vec<&str> = old.unwrap_or_default().split_inclusive('\n').collect();
    let b: Vec<&str> = new.split_inclusive('\n').collect();
    let ops = script(&a, &b);
    let side = |prefix: &str| {
        if shown.starts_with('/') {
            shown.to_owned()
        } else {
            format!("{prefix}{shown}")
        }
    };
    let mut out = match old {
        Some(_) => format!("--- {}\n", side("a/")),
        None => "--- /dev/null\n".to_owned(),
    };
    out.push_str(&format!("+++ {}\n", side("b/")));
    for (start, end) in hunks(&ops) {
        let hunk = &ops[start..end];
        let count = |kind: Kind| hunk.iter().filter(|op| op.kind != kind).count();
        let (old_len, new_len) = (count(Kind::Insert), count(Kind::Delete));
        let from = |at: usize, len: usize| if len == 0 { at } else { at + 1 };
        out.push_str(&format!(
            "@@ -{},{old_len} +{},{new_len} @@\n",
            from(hunk[0].old, old_len),
            from(hunk[0].new, new_len)
        ));
        for op in hunk {
            let (mark, line) = match op.kind {
                Kind::Equal => (' ', a[op.old]),
                Kind::Delete => ('-', a[op.old]),
                Kind::Insert => ('+', b[op.new]),
            };
            out.push(mark);
            out.push_str(line);
            if !line.ends_with('\n') {
                out.push_str("\n\\ No newline at end of file\n");
            }
        }
    }
    out
}

/// The shortest edit script from `a` to `b` by longest common subsequence;
/// deletes come before inserts in a changed run. The common head and tail
/// stay out of the LCS table, so a small edit to a long file stays cheap.
fn script(a: &[&str], b: &[&str]) -> Vec<Op> {
    let head = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let tail = a[head..]
        .iter()
        .rev()
        .zip(b[head..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let equal = |at: usize| Op {
        kind: Kind::Equal,
        old: at,
        new: at + b.len() - a.len(),
    };
    let mut ops: Vec<Op> = (0..head)
        .map(|i| Op {
            kind: Kind::Equal,
            old: i,
            new: i,
        })
        .collect();
    ops.extend(
        lcs_script(&a[head..a.len() - tail], &b[head..b.len() - tail])
            .into_iter()
            .map(|op| Op {
                old: op.old + head,
                new: op.new + head,
                ..op
            }),
    );
    ops.extend((a.len() - tail..a.len()).map(equal));
    ops
}

/// [`script`] over the whole of `a` and `b`.
fn lcs_script(a: &[&str], b: &[&str]) -> Vec<Op> {
    let (n, m) = (a.len(), b.len());
    let width = m + 1;
    let mut lcs = vec![0u32; (n + 1) * width];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i * width + j] = if a[i] == b[j] {
                lcs[(i + 1) * width + j + 1] + 1
            } else {
                lcs[(i + 1) * width + j].max(lcs[i * width + j + 1])
            };
        }
    }
    let mut ops = Vec::with_capacity(n + m);
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        let kind = if i < n && j < m && a[i] == b[j] {
            Kind::Equal
        } else if j == m || (i < n && lcs[(i + 1) * width + j] >= lcs[i * width + j + 1]) {
            Kind::Delete
        } else {
            Kind::Insert
        };
        ops.push(Op {
            kind,
            old: i,
            new: j,
        });
        match kind {
            Kind::Equal => (i, j) = (i + 1, j + 1),
            Kind::Delete => i += 1,
            Kind::Insert => j += 1,
        }
    }
    ops
}

/// `[start, end)` op ranges of each hunk: the changes with up to
/// [`CONTEXT`] equal lines around them, merged when their context touches.
fn hunks(ops: &[Op]) -> Vec<(usize, usize)> {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for (at, op) in ops.iter().enumerate() {
        if op.kind == Kind::Equal {
            continue;
        }
        let start = at.saturating_sub(CONTEXT);
        let end = (at + 1 + CONTEXT).min(ops.len());
        match ranges.last_mut() {
            Some(last) if start <= last.1 => last.1 = end,
            _ => ranges.push((start, end)),
        }
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_text_has_no_diff() {
        assert_eq!(unified("c.toml", Some("a\nb\n"), "a\nb\n"), "");
    }

    #[test]
    fn a_new_file_is_all_added_against_dev_null() {
        assert_eq!(
            unified("$/x.conf", None, "a\nb\n"),
            "--- /dev/null\n+++ b/$/x.conf\n@@ -0,0 +1,2 @@\n+a\n+b\n"
        );
    }

    #[test]
    fn an_absolute_path_has_no_side_prefix() {
        assert_eq!(
            unified("/c/config.toml", Some("a\n"), "b\n"),
            "--- /c/config.toml\n+++ /c/config.toml\n@@ -1,1 +1,1 @@\n-a\n+b\n"
        );
    }

    #[test]
    fn an_empty_file_gains_lines() {
        assert_eq!(
            unified("c.toml", Some(""), "a\n"),
            "--- a/c.toml\n+++ b/c.toml\n@@ -0,0 +1,1 @@\n+a\n"
        );
    }

    #[test]
    fn changes_keep_three_lines_of_context_and_split_far_hunks() {
        let old: String = (1..=20).map(|n| format!("{n}\n")).collect();
        let new = old.replacen("2\n", "two\n", 1).replace("\n18\n", "\n");
        assert_eq!(
            unified("f", Some(&old), &new),
            "--- a/f\n+++ b/f\n\
             @@ -1,5 +1,5 @@\n 1\n-2\n+two\n 3\n 4\n 5\n\
             @@ -15,6 +15,5 @@\n 15\n 16\n 17\n-18\n 19\n 20\n"
        );
    }

    #[test]
    fn close_changes_share_one_hunk() {
        let old = "1\n2\n3\n4\n5\n6\n7\n8\n";
        let new = "1\nx\n3\n4\n5\n6\ny\n8\n";
        assert_eq!(
            unified("f", Some(old), new),
            "--- a/f\n+++ b/f\n@@ -1,8 +1,8 @@\n 1\n-2\n+x\n 3\n 4\n 5\n 6\n-7\n+y\n 8\n"
        );
    }

    #[test]
    fn a_missing_final_newline_is_marked() {
        assert_eq!(
            unified("f", Some("a"), "a\nb\n"),
            "--- a/f\n+++ b/f\n@@ -1,1 +1,2 @@\n-a\n\\ No newline at end of file\n+a\n+b\n"
        );
    }
}
