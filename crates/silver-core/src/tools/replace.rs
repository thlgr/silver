//! Fuzzy find-and-replace for model edits: increasingly permissive matchers let whitespace,
//! indentation, escaping and Unicode drift still land on the intended region. The two
//! similarity-based ones run last, and never under `replace_all`.

use std::borrow::Cow;

/// A byte span `[start, end)` in the ORIGINAL content.
type Span = (usize, usize);

/// One matcher of the chain: `(content, pattern)` → spans in the original content.
type Strategy = fn(&str, &str) -> Vec<Span>;

/// The outcome of a successful replacement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Replacement {
    pub content: String,
    pub matches: usize,
    /// Which strategy matched; anything but `exact` means the file differed from `old_string`.
    pub strategy: &'static str,
    /// 1-based line of the first match in the ORIGINAL content.
    pub line: usize,
}

pub const IDENTICAL_STRINGS_ERROR: &str = "No edit was applied because old_string and new_string are identical. Provide the existing text to replace in old_string and the changed replacement text in new_string.";

const NO_MATCH_ERROR: &str = "Could not find a match for old_string in the file";

/// Typographic Unicode variants mapped to ASCII equivalents (smart quotes, dashes, ellipsis,
/// the space-separator family).
fn ascii_variant(c: char) -> Option<&'static str> {
    Some(match c {
        '\u{201c}' | '\u{201d}' => "\"",
        '\u{2018}' | '\u{2019}' => "'",
        '\u{2014}' => "--",
        '\u{2013}' | '\u{2212}' => "-",
        '\u{2026}' => "...",
        '\u{00a0}' | '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' => " ",
        _ => return None,
    })
}

/// Normalized text with `orig_at[k]`, the original offset of output byte `k` plus a final sentinel,
/// so a span `(s, e)` maps back to `(orig_at[s], orig_at[e])`. A collapsed run maps to its first
/// char, which absorbs trailing collapsed whitespace only when the match ended in it.
struct Normalized {
    text: String,
    orig_at: Vec<usize>,
}

impl Normalized {
    /// `rewrite` returns what a char becomes, or `None` to drop it.
    fn build(original: &str, mut rewrite: impl FnMut(char) -> Option<Cow<'static, str>>) -> Self {
        let mut text = String::with_capacity(original.len());
        let mut orig_at = Vec::with_capacity(original.len() + 1);
        for (offset, c) in original.char_indices() {
            let Some(out) = rewrite(c) else {
                continue;
            };
            for _ in 0..out.len() {
                orig_at.push(offset);
            }
            text.push_str(&out);
        }
        orig_at.push(original.len());
        Self { text, orig_at }
    }

    fn to_original(&self, spans: Vec<Span>) -> Vec<Span> {
        spans
            .into_iter()
            .map(|(s, e)| (self.orig_at[s], self.orig_at[e]))
            .collect()
    }
}

/// Collapse runs of spaces and tabs to a single space, emitted on the run's FIRST char so
/// the span maps back to where the run starts.
fn whitespace_collapsed(text: &str) -> Normalized {
    let mut in_run = false;
    Normalized::build(text, |c| {
        if c == ' ' || c == '\t' {
            if in_run {
                None
            } else {
                in_run = true;
                Some(Cow::Borrowed(" "))
            }
        } else {
            in_run = false;
            Some(Cow::Owned(c.to_string()))
        }
    })
}

fn unicode_normalized(text: &str) -> Normalized {
    Normalized::build(text, |c| {
        Some(match ascii_variant(c) {
            Some(ascii) => Cow::Borrowed(ascii),
            None => Cow::Owned(c.to_string()),
        })
    })
}

fn unicode_normalize(text: &str) -> String {
    unicode_normalized(text).text
}

// ── Line windows ─────────────────────────────────────────────────────────

/// Lines split on '\n' (a trailing newline yields a final empty line, as Python's split).
fn split_lines(text: &str) -> Vec<&str> {
    text.split('\n').collect()
}

/// Byte offset at which each line of `lines` starts in the text they were split from.
fn line_starts(lines: &[&str]) -> Vec<usize> {
    let mut starts = Vec::with_capacity(lines.len());
    let mut offset = 0;
    for line in lines {
        starts.push(offset);
        offset += line.len() + 1;
    }
    starts
}

/// Span covering `lines[i..i + n]`, excluding the newline that ends the last of them.
fn window_span(lines: &[&str], starts: &[usize], i: usize, n: usize) -> Span {
    let last = i + n - 1;
    (starts[i], starts[last] + lines[last].len())
}

/// Spans of every `n`-line window starting at `i` for which `accept(i)` holds.
fn window_spans(lines: &[&str], n: usize, mut accept: impl FnMut(usize) -> bool) -> Vec<Span> {
    if n == 0 || n > lines.len() {
        return Vec::new();
    }
    let starts = line_starts(lines);
    (0..=lines.len() - n)
        .filter(|&i| accept(i))
        .map(|i| window_span(lines, &starts, i, n))
        .collect()
}

/// Match `pattern` against `content` after applying `transform` to each line block.
fn match_transformed_lines(
    content: &str,
    pattern: &str,
    transform: impl Fn(&[&str]) -> Vec<String>,
) -> Vec<Span> {
    let content_lines = split_lines(content);
    let pattern_norm = transform(&split_lines(pattern));
    let n = pattern_norm.len();
    window_spans(&content_lines, n, |i| {
        transform(&content_lines[i..i + n]) == pattern_norm
    })
}

fn strip_boundary(lines: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = lines.iter().map(|line| (*line).to_string()).collect();
    if let Some(first) = out.first_mut() {
        *first = first.trim().to_string();
    }
    if out.len() > 1 {
        if let Some(last) = out.last_mut() {
            *last = last.trim().to_string();
        }
    }
    out
}

// ── Strategies ───────────────────────────────────────────────────────────

/// Strategy 1: exact, non-overlapping occurrences (`str::replace` semantics).
fn strategy_exact(content: &str, pattern: &str) -> Vec<Span> {
    content
        .match_indices(pattern)
        .map(|(i, m)| (i, i + m.len()))
        .collect()
}

/// Strategy 2: strip each line before comparing.
fn strategy_line_trimmed(content: &str, pattern: &str) -> Vec<Span> {
    match_transformed_lines(content, pattern, |lines| {
        lines.iter().map(|line| line.trim().to_string()).collect()
    })
}

/// Strategy 3: collapse runs of spaces/tabs to a single space.
fn strategy_whitespace_normalized(content: &str, pattern: &str) -> Vec<Span> {
    let normalized = whitespace_collapsed(content);
    let pattern_norm = whitespace_collapsed(pattern).text;
    let matches = strategy_exact(&normalized.text, &pattern_norm);
    normalized.to_original(matches)
}

/// Strategy 4: ignore leading indentation entirely.
fn strategy_indentation_flexible(content: &str, pattern: &str) -> Vec<Span> {
    match_transformed_lines(content, pattern, |lines| {
        lines
            .iter()
            .map(|line| line.trim_start().to_string())
            .collect()
    })
}

/// Strategy 5: treat literal `\n` / `\t` / `\r` in the pattern as control characters.
fn strategy_escape_normalized(content: &str, pattern: &str) -> Vec<Span> {
    let unescaped = pattern
        .replace("\\n", "\n")
        .replace("\\t", "\t")
        .replace("\\r", "\r");
    if unescaped == pattern {
        return Vec::new();
    }
    strategy_exact(content, &unescaped)
}

/// Strategy 6: strip whitespace on the first and last lines only.
fn strategy_trimmed_boundary(content: &str, pattern: &str) -> Vec<Span> {
    match_transformed_lines(content, pattern, strip_boundary)
}

/// Strategy 7: exact / line-trimmed match after Unicode→ASCII normalization of both sides.
fn strategy_unicode_normalized(content: &str, pattern: &str) -> Vec<Span> {
    let norm_pattern = unicode_normalize(pattern);
    let normalized = unicode_normalized(content);
    if normalized.text == content && norm_pattern == pattern {
        return Vec::new();
    }
    let mut matches = strategy_exact(&normalized.text, &norm_pattern);
    if matches.is_empty() {
        matches = strategy_line_trimmed(&normalized.text, &norm_pattern);
    }
    normalized.to_original(matches)
}

/// Strategy 8: anchor on the first and last lines, similarity-score the middle.
fn strategy_block_anchor(content: &str, pattern: &str) -> Vec<Span> {
    let norm_pattern = unicode_normalize(pattern);
    let pattern_lines = split_lines(&norm_pattern);
    let n = pattern_lines.len();
    if n < 2 {
        return Vec::new();
    }
    let first = pattern_lines[0].trim();
    let last = pattern_lines[n - 1].trim();
    // Match on normalized lines; compute offsets from the ORIGINAL lines so multi-char
    // expansions (em dash → "--") do not shift positions.
    let norm_content = unicode_normalize(content);
    let norm_lines = split_lines(&norm_content);
    let content_lines = split_lines(content);
    if n > norm_lines.len() || norm_lines.len() != content_lines.len() {
        return Vec::new();
    }
    let candidates: Vec<usize> = (0..=norm_lines.len() - n)
        .filter(|&i| norm_lines[i].trim() == first && norm_lines[i + n - 1].trim() == last)
        .collect();
    // Looser thresholds matched unrelated blocks; these are the safe floor.
    let threshold = if candidates.len() == 1 { 0.50 } else { 0.70 };
    let pattern_middle = pattern_lines[1..n - 1].join("\n");
    window_spans(&content_lines, n, |i| {
        if !candidates.contains(&i) {
            return false;
        }
        if n <= 2 {
            return true;
        }
        let content_middle = norm_lines[i + 1..i + n - 1].join("\n");
        similarity(&content_middle, &pattern_middle) >= threshold
    })
}

/// Strategy 9 (last resort): anchored per-line similarity, every non-blank line ≥ 0.80.
fn strategy_context_aware(content: &str, pattern: &str) -> Vec<Span> {
    let pattern_lines = split_lines(pattern);
    let content_lines = split_lines(content);
    let n = pattern_lines.len();
    if n > content_lines.len() {
        return Vec::new();
    }
    let first = pattern_lines[0].trim();
    let last = pattern_lines[n - 1].trim();
    let similar = |a: &str, b: &str| a == b || similarity(a, b) >= 0.80;
    window_spans(&content_lines, n, |i| {
        let block = &content_lines[i..i + n];
        if !similar(first, block[0].trim()) || !similar(last, block[n - 1].trim()) {
            return false;
        }
        pattern_lines
            .iter()
            .zip(block)
            .all(|(p, c)| p.trim().is_empty() || similar(p.trim(), c.trim()))
    })
}

/// Ordered chain: precise strategies first, similarity-based last.
const STRATEGIES: &[(&str, Strategy)] = &[
    ("exact", strategy_exact),
    ("line_trimmed", strategy_line_trimmed),
    ("whitespace_normalized", strategy_whitespace_normalized),
    ("indentation_flexible", strategy_indentation_flexible),
    ("escape_normalized", strategy_escape_normalized),
    ("trimmed_boundary", strategy_trimmed_boundary),
    ("unicode_normalized", strategy_unicode_normalized),
    ("block_anchor", strategy_block_anchor),
    ("context_aware", strategy_context_aware),
];

/// Matches from these only approximately resemble `old_string`: fine for one unique
/// replacement, never safe under `replace_all`.
const SIMILARITY_STRATEGIES: &[&str] = &["block_anchor", "context_aware"];

// ── Similarity ───────────────────────────────────────────────────────────

/// Ratcliff/Obershelp similarity in `[0, 1]`, the ratio Python's `SequenceMatcher` reports:
/// twice the characters in recursively found longest common substrings over the total length.
pub fn similarity(a: &str, b: &str) -> f64 {
    if a == b {
        return 1.0;
    }
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let total = a.len() + b.len();
    if total == 0 {
        return 1.0;
    }
    2.0 * matching_chars(&a, &b) as f64 / total as f64
}

fn matching_chars(a: &[char], b: &[char]) -> usize {
    if a.is_empty() || b.is_empty() {
        return 0;
    }
    let (ai, bi, len) = longest_common_substring(a, b);
    if len == 0 {
        return 0;
    }
    len + matching_chars(&a[..ai], &b[..bi]) + matching_chars(&a[ai + len..], &b[bi + len..])
}

/// `(start in a, start in b, length)` of the longest common substring; earliest in `a` on ties.
fn longest_common_substring(a: &[char], b: &[char]) -> (usize, usize, usize) {
    let mut best = (0, 0, 0);
    let mut prev = vec![0usize; b.len() + 1];
    let mut curr = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        for (j, &cb) in b.iter().enumerate() {
            curr[j + 1] = if ca == cb { prev[j] + 1 } else { 0 };
            if curr[j + 1] > best.2 {
                best = (i + 1 - curr[j + 1], j + 1 - curr[j + 1], curr[j + 1]);
            }
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    best
}

// ── Orchestrator ─────────────────────────────────────────────────────────

/// Whether the edit is already there (a re-sent edit succeeds as a no-op): `new_string` of at least
/// 8 trimmed chars present exactly and `old_string` gone.
pub fn is_already_applied(content: &str, old_string: &str, new_string: &str) -> bool {
    if new_string.trim().chars().count() < 8 || !content.contains(new_string) {
        return false;
    }
    old_string == new_string || !content.contains(old_string)
}

fn matched_regions(content: &str, matches: &[Span]) -> String {
    matches.iter().map(|&(s, e)| &content[s..e]).collect()
}

fn line_number_at(content: &str, offset: usize) -> usize {
    content[..offset].matches('\n').count() + 1
}

/// Up to `cap` match positions as `L<line>: <snippet>` rows.
fn format_match_locations(content: &str, matches: &[Span], cap: usize) -> String {
    let mut rows = Vec::new();
    for &(start, _) in matches.iter().take(cap) {
        let line_start = content[..start].rfind('\n').map_or(0, |i| i + 1);
        let line_end = content[line_start..]
            .find('\n')
            .map_or(content.len(), |i| line_start + i);
        let mut snippet = content[line_start..line_end].trim().to_string();
        if snippet.chars().count() > 80 {
            snippet = snippet.chars().take(77).collect::<String>() + "...";
        }
        rows.push(format!("  L{}: {snippet}", line_number_at(content, start)));
    }
    if matches.len() > cap {
        rows.push(format!("  ... and {} more", matches.len() - cap));
    }
    rows.join("\n")
}

/// Find and replace via the strategy chain. `Err` is the message the model reads: why the input is
/// empty or unchanged, where an ambiguous match is, escape drift, or the closest lines.
pub fn fuzzy_find_and_replace(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Result<Replacement, String> {
    if old_string.is_empty() {
        // A terse "cannot be empty" leaves the model re-sending the identical call until the
        // loop detector kills the run.
        return Err("old_string is empty — nothing to match. Set old_string to the exact existing text the replacement should replace (read the file first if unsure). To create a new file or fully rewrite one, use write_file instead. Do not re-send this call unchanged.".to_string());
    }
    if old_string.trim().is_empty() {
        return Err("old_string is only whitespace — provide non-blank text to match. Set it to the exact existing text the replacement should replace (read the file first if unsure). Do not re-send this call unchanged.".to_string());
    }
    if old_string == new_string {
        return Err(IDENTICAL_STRINGS_ERROR.to_string());
    }

    for (name, strategy) in STRATEGIES {
        let matches = strategy(content, old_string);
        if matches.is_empty() {
            continue;
        }
        if matches.len() > 1 && !replace_all {
            return Err(format!(
                "Found {} matches for old_string. Provide more context to make it unique, or use replace_all=true. Matches:\n{}",
                matches.len(),
                format_match_locations(content, &matches, 5)
            ));
        }
        if replace_all && matches.len() > 1 && SIMILARITY_STRATEGIES.contains(name) {
            return Err(format!(
                "Found {} approximate matches via the '{name}' strategy; replace_all only applies to exact matches. Provide the precise text (whitespace included) so an exact/line-trimmed match can be made.",
                matches.len()
            ));
        }
        // Non-exact matches came through some normalization, so new_string may carry
        // serialization drift the file does not have.
        if *name != "exact" {
            if let Some(drift) = detect_escape_drift(content, &matches, old_string, new_string) {
                return Err(drift);
            }
        }
        let effective_new = maybe_unescape_new_string(new_string, content, &matches);
        let reindent_from = (*name != "exact").then_some(old_string);
        let line = line_number_at(content, matches[0].0);
        let content = apply_replacements(content, &matches, &effective_new, reindent_from);
        return Ok(Replacement {
            content,
            matches: matches.len(),
            strategy: name,
            line,
        });
    }

    Err(NO_MATCH_ERROR.to_string())
}

// ── Escape-drift guards ──────────────────────────────────────────────────

/// Error text when `new_string` carries tool-call escape artifacts: `\'` / `\"` in both
/// strings but not in the matched region, or every backslash run doubled.
fn detect_escape_drift(
    content: &str,
    matches: &[Span],
    old_string: &str,
    new_string: &str,
) -> Option<String> {
    let has_quote_suspects = new_string.contains("\\'") || new_string.contains("\\\"");
    if !has_quote_suspects && !old_string.contains('\\') {
        return None;
    }
    let regions = matched_regions(content, matches);
    if has_quote_suspects {
        for suspect in ["\\'", "\\\""] {
            if new_string.contains(suspect)
                && old_string.contains(suspect)
                && !regions.contains(suspect)
            {
                let plain = &suspect[1..];
                return Some(format!(
                    "Escape-drift detected: old_string and new_string contain the literal sequence {suspect:?} but the matched region of the file does not. This is almost always a tool-call serialization artifact where an apostrophe or quote got prefixed with a spurious backslash. Re-read the file with read_file and pass old_string/new_string without backslash-escaping {plain:?} characters."
                ));
            }
        }
    }
    detect_backslash_doubling(&regions, old_string, new_string)
}

/// Lengths of the maximal backslash runs in `s`, in order.
fn backslash_runs(s: &str) -> Vec<usize> {
    let mut runs = Vec::new();
    let mut current = 0;
    for c in s.chars() {
        if c == '\\' {
            current += 1;
        } else if current > 0 {
            runs.push(current);
            current = 0;
        }
    }
    if current > 0 {
        runs.push(current);
    }
    runs
}

/// Detect an `old_string` whose every backslash run is exactly twice the file's (the
/// arguments were JSON-escaped one extra time).
fn detect_backslash_doubling(regions: &str, old_string: &str, new_string: &str) -> Option<String> {
    let old_runs = backslash_runs(old_string);
    let file_runs = backslash_runs(regions);
    let doubled = !old_runs.is_empty()
        && old_runs.len() == file_runs.len()
        && old_runs != file_runs
        && old_runs.iter().zip(&file_runs).all(|(o, f)| *o == f * 2)
        && (file_runs.iter().any(|f| *f >= 2) || file_runs.len() >= 2)
        && backslash_runs(new_string) != file_runs;
    doubled.then(|| {
        "Escape-drift detected: every backslash run in old_string is exactly twice as long as in the matched region of the file (e.g. the file has `\\\\` where old_string has `\\\\\\\\`). The tool-call arguments were JSON-escaped one extra time; applying new_string verbatim would double every backslash in the file. Re-read the file with read_file and resend old_string/new_string with the backslash counts exactly as they appear in the file.".to_string()
    })
}

/// Convert literal `\t` / `\r` in `new_string` to control chars, per sequence, only when
/// the matched region already contains the real control char. `\n` is excluded: rewriting
/// it would mangle source escape literals.
fn maybe_unescape_new_string(new_string: &str, content: &str, matches: &[Span]) -> String {
    if !new_string.contains("\\t") && !new_string.contains("\\r") {
        return new_string.to_string();
    }
    let regions = matched_regions(content, matches);
    let mut out = new_string.to_string();
    for (literal, control) in [("\\t", "\t"), ("\\r", "\r")] {
        if out.contains(literal) && regions.contains(control) {
            out = out.replace(literal, control);
        }
    }
    out
}

// ── Replacement shaping ──────────────────────────────────────────────────

fn leading_whitespace(line: &str) -> &str {
    &line[..line.len() - line.trim_start_matches([' ', '\t']).len()]
}

fn first_meaningful_line(text: &str) -> Option<&str> {
    text.split('\n').find(|line| !line.trim().is_empty())
}

/// Re-anchor `new_string`'s indentation onto the file's actual base indent after a non-exact
/// match: swap the base prefix of `old_string`'s first non-blank line for the file's,
/// preserving relative nesting; shallower lines anchor to the file base.
fn reindent_replacement(file_region: &str, old_string: &str, new_string: &str) -> String {
    if new_string.is_empty() {
        return String::new();
    }
    let (Some(old_first), Some(file_first)) = (
        first_meaningful_line(old_string),
        first_meaningful_line(file_region),
    ) else {
        return new_string.to_string();
    };
    let old_indent = leading_whitespace(old_first);
    let file_indent = leading_whitespace(file_first);
    if old_indent == file_indent {
        return new_string.to_string();
    }
    new_string
        .split('\n')
        .map(|line| {
            if line.trim().is_empty() {
                line.to_string()
            } else if leading_whitespace(line).starts_with(old_indent) {
                format!("{file_indent}{}", &line[old_indent.len()..])
            } else {
                format!("{file_indent}{}", line.trim_start_matches([' ', '\t']))
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Splice `new_string` over each span, end to start so offsets stay valid; a non-exact
/// match (`reindent_from` set) re-indents it per region.
fn apply_replacements(
    content: &str,
    matches: &[Span],
    new_string: &str,
    reindent_from: Option<&str>,
) -> String {
    let mut spans = matches.to_vec();
    spans.sort_by_key(|&(start, _)| std::cmp::Reverse(start));
    let mut result = content.to_string();
    for (start, end) in spans {
        let adjusted = match reindent_from {
            Some(old_string) => reindent_replacement(&content[start..end], old_string, new_string),
            None => new_string.to_string(),
        };
        result.replace_range(start..end, &adjusted);
    }
    result
}

// ── "Did you mean?" diagnostics ──────────────────────────────────────────

/// Render the leading whitespace run visibly (→ = tab, · = space).
fn visualize_whitespace(line: &str) -> String {
    let stripped = line.trim_start_matches([' ', '\t']);
    let prefix = &line[..line.len() - stripped.len()];
    format!("{}{stripped}", prefix.replace('\t', "→").replace(' ', "·"))
}

/// Numbered snippets of the lines most similar to `old_string`'s anchor line, or empty.
pub fn find_closest_lines(old_string: &str, content: &str) -> String {
    const CONTEXT_LINES: usize = 2;
    const MAX_RESULTS: usize = 3;
    let old_lines: Vec<&str> = old_string.lines().collect();
    let content_lines: Vec<&str> = content.lines().collect();
    if old_lines.is_empty() || content_lines.is_empty() {
        return String::new();
    }
    let anchor = match old_lines
        .iter()
        .map(|line| line.trim())
        .find(|line| !line.is_empty())
    {
        Some(anchor) => anchor,
        None => return String::new(),
    };

    let mut scored: Vec<(f64, usize)> = content_lines
        .iter()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(i, line)| (similarity(anchor, line.trim()), i))
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let top: Vec<(f64, usize)> = scored
        .into_iter()
        .filter(|(score, _)| *score > 0.3)
        .take(MAX_RESULTS)
        .collect();
    if top.is_empty() {
        return String::new();
    }

    let mut parts = Vec::new();
    let mut seen = Vec::new();
    for &(_, line_idx) in &top {
        let start = line_idx.saturating_sub(CONTEXT_LINES);
        let end = (line_idx + old_lines.len() + CONTEXT_LINES).min(content_lines.len());
        if seen.contains(&(start, end)) {
            continue;
        }
        seen.push((start, end));
        let snippet: Vec<String> = (start..end)
            .map(|j| format!("{:4}| {}", j + 1, content_lines[j]))
            .collect();
        parts.push(snippet.join("\n"));
    }
    let mut result = parts.join("\n---\n");

    // Whitespace-shaped miss: the best line equals the anchor once stripped. Show both with
    // visible leading whitespace so the model copies the file's.
    let best_line = content_lines[top[0].1];
    if best_line.trim() == anchor && best_line != old_lines[0] {
        result.push_str(&format!(
            "\n\nWhitespace difference detected (→ = tab, · = space):\n  file has: {}\n  you sent: {}\nUse the exact whitespace shown in 'file has'.",
            visualize_whitespace(best_line),
            visualize_whitespace(old_lines[0])
        ));
    }
    result
}

/// The `Did you mean...` suffix for a plain no-match error, else empty (an ambiguous,
/// escape-drift or identical error also has no matches, but a hint would mislead).
pub fn no_match_hint(error: &str, old_string: &str, content: &str) -> String {
    if !error.starts_with("Could not find") {
        return String::new();
    }
    let hint = find_closest_lines(old_string, content);
    if hint.is_empty() {
        String::new()
    } else {
        format!("\n\nDid you mean one of these sections?\n{hint}")
    }
}
