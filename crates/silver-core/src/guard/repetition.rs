//! Repetition guard for the truncated-reply continuation: a model looping on one fragment can
//! spend its whole output budget, and continuing would stitch that into the answer. Only long
//! verbatim repeats covering most of the fragment trip it.

use std::collections::{HashMap, HashSet};

/// Below this length the check does not run: short truncations trivially contain repeated
/// tokens and are legitimately continued.
pub const MIN_FRAGMENT_LENGTH: usize = 400;

/// Exact-repeat window; far beyond ordinary phrasing reuse (citations, headings, similar code).
pub const REPEAT_WINDOW: usize = 60;

/// A window repeating at least this often is a signal even for short fragments.
pub const MIN_REPEAT_COUNT: usize = 5;

/// Repetition-dominated means repeated windows cover at least this fraction.
pub const DOMINANCE_RATIO: f64 = 0.5;

/// A multi-line runaway must be mostly copies of a few lines: at most this fraction of its
/// non-empty lines may be distinct.
pub const RUNAWAY_DISTINCT_LINE_RATIO: f64 = 0.5;

/// What an interrupt checkpoint says instead of a repetition-dominated partial. Replaying the
/// looped bytes would re-seed the loop on the next request; the model only needs to know the
/// reply degenerated and was cut off.
pub const REPETITION_LOOP_INTERRUPTED: &str =
    "[the reply degenerated into a repetition loop and was interrupted]";

/// True when a single 60+ char substring recurs often enough to cover at least half of the
/// text: the signature of a repetition loop. Fails open for short input.
pub fn is_repetition_dominated(text: &str) -> bool {
    let n = text.chars().count();
    if n < MIN_FRAGMENT_LENGTH {
        return false;
    }

    // Fast path: one normalized line duplicated enough to cover half the fragment.
    if line_repetition_dominated(text, n) {
        return true;
    }

    // General path: fixed-size windows sliding one char at a time, catching loops that do not
    // align to line boundaries. A window must appear needed times to cover at least
    // DOMINANCE_RATIO of the fragment and at least MIN_REPEAT_COUNT times.
    let window = REPEAT_WINDOW;
    let needed =
        MIN_REPEAT_COUNT.max(((n as f64) * DOMINANCE_RATIO / window as f64).ceil() as usize);
    if n < window {
        return false;
    }
    let chars: Vec<char> = text.chars().collect();
    let mut counts: HashMap<&[char], usize> = HashMap::new();
    for i in 0..=(n - window) {
        let key = &chars[i..i + window];
        let count = counts.entry(key).or_insert(0);
        *count += 1;
        if *count >= needed {
            return true;
        }
    }
    false
}

/// Stricter than is_repetition_dominated, since an interrupt checkpoint drops the partial: repeats
/// must dominate and, with line structure, at most half the non-empty lines may be distinct.
pub fn is_runaway_repetition(text: &str) -> bool {
    if !is_repetition_dominated(text) {
        return false;
    }
    let lines: Vec<&str> = py_splitlines(text)
        .into_iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect();
    if lines.len() < MIN_REPEAT_COUNT {
        return true; // no line structure to judge by: a dominated single-line loop
    }
    let distinct: HashSet<&str> = lines.iter().copied().collect();
    distinct.len() as f64 <= lines.len() as f64 * RUNAWAY_DISTINCT_LINE_RATIO
}

/// True when a single normalized line covers half the fragment via repeats.
fn line_repetition_dominated(text: &str, n: usize) -> bool {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for raw in py_splitlines(text) {
        let norm = raw.trim();
        if !norm.is_empty() {
            *counts.entry(norm).or_insert(0) += 1;
        }
    }
    counts.iter().any(|(line, count)| {
        *count >= MIN_REPEAT_COUNT
            && (*count * line.chars().count()) as f64 >= (n as f64) * DOMINANCE_RATIO
    })
}

/// True for the characters Python's str.splitlines treats as line boundaries.
fn is_line_break(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{001c}'
            | '\u{001d}'
            | '\u{001e}'
            | '\u{0085}'
            | '\u{2028}'
            | '\u{2029}'
    )
}

/// Split like Python's str.splitlines: recognizes the wider set of line boundaries and treats
/// a carriage-return / line-feed pair as a single break, with no trailing empty line.
fn py_splitlines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    let mut chars = text.char_indices().peekable();
    while let Some((idx, c)) = chars.next() {
        if !is_line_break(c) {
            continue;
        }
        lines.push(&text[start..idx]);
        if c == '\r' {
            if let Some(&(next_idx, '\n')) = chars.peek() {
                chars.next();
                start = next_idx + 1;
                continue;
            }
        }
        start = idx + c.len_utf8();
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}
