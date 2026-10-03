//! Line-shift map for diagnostics across an edit: inserted or deleted lines move everything below,
//! so a baseline is mapped to post-edit lines before the set difference, and diagnostics in a
//! deleted region drop out. The range stays in the key to keep "same error, new line".

use serde_json::{json, Value};

/// A pre-to-post line map over 0-indexed lines (LSP convention). None means the
/// line was deleted.
pub type LineShift = Box<dyn Fn(i64) -> Option<i64> + Send + Sync>;

/// Cap on the LCS dynamic-programming table. A larger changed region is treated
/// as one whole-region replacement, which is conservative but never wrong: it
/// can only drop diagnostics, never invent a mapping.
const MAX_DIFF_CELLS: usize = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpTag {
    Equal,
    Delete,
    Insert,
}

#[derive(Clone, Copy, Debug)]
struct Opcode {
    tag: OpTag,
    i1: usize,
    i2: usize,
    j1: usize,
    j2: usize,
}

/// Split text into lines the way Python's splitlines does for our purposes:
/// split on newline, drop the trailing empty line.
fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        Vec::new()
    } else {
        text.lines().collect()
    }
}

/// Build a shift closure over the two texts. One diff up front; the closure
/// scans the (small) opcode list per lookup.
pub fn build_line_shift(pre_text: &str, post_text: &str) -> LineShift {
    let pre_lines = split_lines(pre_text);
    let post_lines = split_lines(post_text);
    let post_len = post_lines.len();
    if pre_lines == post_lines {
        return Box::new(Some);
    }
    let opcodes = diff_opcodes(&pre_lines, &post_lines);
    Box::new(move |line: i64| {
        for op in &opcodes {
            let i1 = op.i1 as i64;
            let i2 = op.i2 as i64;
            if i1 <= line && line < i2 {
                return if op.tag == OpTag::Equal {
                    Some(line - i1 + op.j1 as i64)
                } else {
                    None
                };
            }
            if line < i1 {
                break;
            }
        }
        // Past the last pre line: anchor at the end of the post text.
        if post_len == 0 {
            None
        } else {
            Some(post_len as i64 - 1)
        }
    })
}

/// Compute opcodes for the full texts.
fn diff_opcodes(pre: &[&str], post: &[&str]) -> Vec<Opcode> {
    let mut prefix = 0usize;
    while prefix < pre.len() && prefix < post.len() && pre[prefix] == post[prefix] {
        prefix += 1;
    }
    let mut suffix = 0usize;
    while suffix < pre.len() - prefix
        && suffix < post.len() - prefix
        && pre[pre.len() - 1 - suffix] == post[post.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let mut ops = Vec::new();
    if prefix > 0 {
        ops.push(Opcode {
            tag: OpTag::Equal,
            i1: 0,
            i2: prefix,
            j1: 0,
            j2: prefix,
        });
    }
    let a = &pre[prefix..pre.len() - suffix];
    let b = &post[prefix..post.len() - suffix];
    if a.is_empty() {
        if !b.is_empty() {
            ops.push(Opcode {
                tag: OpTag::Insert,
                i1: prefix,
                i2: prefix,
                j1: prefix,
                j2: prefix + b.len(),
            });
        }
    } else if b.is_empty() {
        ops.push(Opcode {
            tag: OpTag::Delete,
            i1: prefix,
            i2: prefix + a.len(),
            j1: prefix,
            j2: prefix,
        });
    } else {
        ops.extend(lcs_opcodes(a, b, prefix));
    }
    if suffix > 0 {
        ops.push(Opcode {
            tag: OpTag::Equal,
            i1: pre.len() - suffix,
            i2: pre.len(),
            j1: post.len() - suffix,
            j2: post.len(),
        });
    }
    ops
}

/// LCS alignment of the changed middle, with indices offset by base.
fn lcs_opcodes(a: &[&str], b: &[&str], base: usize) -> Vec<Opcode> {
    let n = a.len();
    let m = b.len();
    if n.saturating_mul(m) > MAX_DIFF_CELLS {
        return vec![
            Opcode {
                tag: OpTag::Delete,
                i1: base,
                i2: base + n,
                j1: base,
                j2: base,
            },
            Opcode {
                tag: OpTag::Insert,
                i1: base + n,
                i2: base + n,
                j1: base,
                j2: base + m,
            },
        ];
    }
    let width = m + 1;
    let index = |i: usize, j: usize| i * width + j;
    let mut dp = vec![0u32; (n + 1) * (m + 1)];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[index(i, j)] = if a[i] == b[j] {
                dp[index(i + 1, j + 1)] + 1
            } else {
                dp[index(i + 1, j)].max(dp[index(i, j + 1)])
            };
        }
    }
    let mut raw: Vec<Opcode> = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i] == b[j] {
            raw.push(Opcode {
                tag: OpTag::Equal,
                i1: base + i,
                i2: base + i + 1,
                j1: base + j,
                j2: base + j + 1,
            });
            i += 1;
            j += 1;
        } else if dp[index(i + 1, j)] >= dp[index(i, j + 1)] {
            raw.push(Opcode {
                tag: OpTag::Delete,
                i1: base + i,
                i2: base + i + 1,
                j1: base + j,
                j2: base + j,
            });
            i += 1;
        } else {
            raw.push(Opcode {
                tag: OpTag::Insert,
                i1: base + i,
                i2: base + i,
                j1: base + j,
                j2: base + j + 1,
            });
            j += 1;
        }
    }
    while i < n {
        raw.push(Opcode {
            tag: OpTag::Delete,
            i1: base + i,
            i2: base + i + 1,
            j1: base + j,
            j2: base + j,
        });
        i += 1;
    }
    while j < m {
        raw.push(Opcode {
            tag: OpTag::Insert,
            i1: base + i,
            i2: base + i,
            j1: base + j,
            j2: base + j + 1,
        });
        j += 1;
    }
    merge_runs(raw)
}

/// Merge adjacent contiguous ops that share a tag so the opcode list stays small.
fn merge_runs(raw: Vec<Opcode>) -> Vec<Opcode> {
    let mut merged: Vec<Opcode> = Vec::new();
    for op in raw {
        if let Some(last) = merged.last_mut() {
            if last.tag == op.tag && last.i2 == op.i1 && last.j2 == op.j1 {
                last.i2 = op.i2;
                last.j2 = op.j2;
                continue;
            }
        }
        merged.push(op);
    }
    merged
}

/// diag with its line range remapped; None if the start line was deleted. A multi-line
/// diagnostic whose end straddles a deletion collapses to a single-line range at the
/// shifted start so it stays in the baseline.
pub fn shift_diagnostic_range<F>(mut diag: Value, shift: &F) -> Option<Value>
where
    F: Fn(i64) -> Option<i64> + ?Sized,
{
    let range = diag.get("range");
    let start = range.and_then(|value| value.get("start"));
    let end = range.and_then(|value| value.get("end"));
    let pre_start_line = start
        .and_then(|value| value.get("line"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let new_start_line = shift(pre_start_line)?;
    let new_end_line = end
        .and_then(|value| value.get("line"))
        .and_then(Value::as_i64)
        .and_then(shift)
        .unwrap_or(new_start_line);
    let start_character = start
        .and_then(|value| value.get("character"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let end_character = end
        .and_then(|value| value.get("character"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let object = diag.as_object_mut()?;
    object.insert(
        "range".to_string(),
        json!({
            "start": {"line": new_start_line, "character": start_character},
            "end": {"line": new_end_line, "character": end_character},
        }),
    );
    Some(diag)
}

/// Apply shift to every diagnostic in a baseline, dropping deleted entries.
pub fn shift_baseline<F>(baseline: Vec<Value>, shift: &F) -> Vec<Value>
where
    F: Fn(i64) -> Option<i64> + ?Sized,
{
    baseline
        .into_iter()
        .filter(|value| value.is_object())
        .filter_map(|value| shift_diagnostic_range(value, shift))
        .collect()
}

/// The post-edit lines an edit touched, as half-open 0-indexed ranges. Each range reaches one
/// line past the change, where a server reports what an unfinished line broke; a deletion
/// marks the line the removal joined.
pub fn changed_lines(pre_text: &str, post_text: &str) -> Vec<std::ops::Range<i64>> {
    let pre_lines = split_lines(pre_text);
    let post_lines = split_lines(post_text);
    diff_opcodes(&pre_lines, &post_lines)
        .into_iter()
        .filter(|op| op.tag != OpTag::Equal)
        .map(|op| op.j1 as i64..op.j2 as i64 + 1)
        .collect()
}
