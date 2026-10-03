//! Unified-diff application: an optional `---`/`+++` preamble, then `@@` hunks, all or nothing.

struct Hunk {
    /// 1-based start line in the original file, used when a hunk only adds lines.
    old_start: usize,
    /// Hunk body lines as (prefix, content) where prefix is ' ', '-' or '+'.
    lines: Vec<(char, String)>,
}

/// Apply a unified diff, all or nothing, keeping `original`'s trailing newline.
pub fn apply_unified_diff(original: &str, patch: &str) -> Result<String, String> {
    let hunks = parse_hunks(patch)?;
    if hunks.is_empty() {
        return Err("patch contains no hunks".to_string());
    }

    let ends_with_newline = original.ends_with('\n');
    let lines: Vec<&str> = original.lines().collect();

    let mut output: Vec<&str> = Vec::new();
    let mut search_from = 0usize;

    for (index, hunk) in hunks.iter().enumerate() {
        let old_block: Vec<&str> = hunk
            .lines
            .iter()
            .filter(|(prefix, _)| *prefix != '+')
            .map(|(_, content)| content.as_str())
            .collect();

        if old_block.is_empty() {
            // Addition-only hunk: insert relative to the declared old start, adjusted by
            // the net line count already inserted or removed by earlier hunks.
            let delta = output.len() as isize - search_from as isize;
            let insert_at = (hunk.old_start as isize + delta).max(search_from as isize);
            let insert_at = (insert_at.max(0) as usize).min(lines.len());
            output.extend(&lines[search_from..insert_at]);
            for (prefix, content) in &hunk.lines {
                if *prefix == '+' {
                    output.push(content);
                }
            }
            search_from = insert_at;
            continue;
        }

        let found = find_block(&lines, search_from, &old_block).ok_or_else(|| {
            format!(
                "hunk {} does not apply: expected context starting at or after original line {}",
                index + 1,
                search_from + 1
            )
        })?;
        output.extend(&lines[search_from..found]);
        for (prefix, content) in &hunk.lines {
            match prefix {
                ' ' | '+' => output.push(content),
                '-' => {}
                _ => {}
            }
        }
        search_from = found + old_block.len();
    }
    output.extend(&lines[search_from..]);

    let mut result = output.join("\n");
    if ends_with_newline && !output.is_empty() {
        result.push('\n');
    }
    Ok(result)
}

/// Find 'block' in 'lines' at or after 'from'.
fn find_block(lines: &[&str], from: usize, block: &[&str]) -> Option<usize> {
    if block.is_empty() {
        return Some(from);
    }
    if from + block.len() > lines.len() {
        return None;
    }
    (from..=lines.len() - block.len()).find(|&i| &lines[i..i + block.len()] == block)
}

/// Parse every hunk header and body line, ignoring the optional diff preamble.
fn parse_hunks(patch: &str) -> Result<Vec<Hunk>, String> {
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut current: Option<Hunk> = None;

    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("@@") {
            if let Some(hunk) = current.take() {
                hunks.push(hunk);
            }
            current = Some(Hunk {
                old_start: parse_hunk_header(rest)?,
                lines: Vec::new(),
            });
            continue;
        }

        let Some(hunk) = current.as_mut() else {
            // Preamble such as '--- a/file', '+++ b/file', 'diff --git ...', 'index ...'.
            continue;
        };

        // An empty physical line is never a valid hunk body line; context for an empty
        // line is a single space. Skip it so a trailing blank line cannot break a patch.
        if line.is_empty() {
            continue;
        }
        let mut chars = line.chars();
        let prefix = chars.next().expect("line is non-empty");
        let content = chars.as_str();
        match prefix {
            ' ' | '-' | '+' => hunk.lines.push((prefix, content.to_string())),
            // '\ No newline at end of file' marker: not part of the content.
            '\\' => {}
            other => {
                return Err(format!(
                    "unexpected line inside hunk (starts with {other:?}): {}",
                    line.trim_end()
                ))
            }
        }
    }

    if let Some(hunk) = current.take() {
        hunks.push(hunk);
    }
    Ok(hunks)
}

/// Parse the old start line from the text following '@@'.
fn parse_hunk_header(rest: &str) -> Result<usize, String> {
    let trimmed = rest.trim_start();
    let after_minus = trimmed
        .strip_prefix('-')
        .ok_or_else(|| format!("malformed hunk header: expected '-' range in '@@{rest}'"))?;
    let (old_start, remainder) = take_number(after_minus)
        .ok_or_else(|| format!("malformed hunk header: missing old start line in '@@{rest}'"))?;
    let (_, remainder) = take_optional_len(remainder);
    let after_space = remainder.trim_start();
    let after_plus = after_space
        .strip_prefix('+')
        .ok_or_else(|| format!("malformed hunk header: expected '+' range in '@@{rest}'"))?;
    let (_new_start, _) = take_number(after_plus)
        .ok_or_else(|| format!("malformed hunk header: missing new start line in '@@{rest}'"))?;
    Ok(old_start)
}

fn take_number(input: &str) -> Option<(usize, &str)> {
    let end = input
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(input.len());
    if end == 0 {
        return None;
    }
    let value = input[..end].parse::<usize>().ok()?;
    Some((value, &input[end..]))
}

fn take_optional_len(input: &str) -> (usize, &str) {
    if let Some(rest) = input.strip_prefix(',') {
        if let Some((value, remainder)) = take_number(rest) {
            return (value, remainder);
        }
    }
    (1, input)
}
