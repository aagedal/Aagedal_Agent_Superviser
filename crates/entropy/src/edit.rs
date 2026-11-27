//! Forgiving multi-strategy file editor and concise unified diff generator.
//!
//! Local models frequently get indentation, trailing whitespace, or a middle line slightly off
//! when calling `edit`. Rather than failing immediately and wasting 1–2 round-trips, we try a
//! safe cascade of match strategies before reporting an error:
//!
//! 1. **Exact match** (verbatim substring).
//! 2. **Line-trimmed match** (ignores leading/trailing horizontal whitespace per line).
//! 3. **Indentation-normalized match** (dedents both `old_string` and candidate window, then
//!    re-indents `new_string` to match the file's actual indentation).
//! 4. **Block-anchor match** (for ≥3-line blocks: anchors on trimmed first and last lines when
//!    the interior has high line similarity and the match is unique).
//!
//! When all strategies miss, we locate the closest line window in the file and include a short
//! snippet in the error so the model can correct its call without re-reading the whole file.
//! Multi-hunk edits (`apply_multi_edit`) are applied atomically in memory first.

use serde::Deserialize;

/// One replacement hunk for [`apply_multi_edit`].
#[derive(Debug, Clone, Deserialize)]
pub struct EditSpec {
    pub old_string: String,
    pub new_string: String,
    #[serde(default)]
    pub replace_all: bool,
}

/// Which strategy matched an edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchStrategy {
    Exact,
    LineTrimmed,
    IndentationNormalized,
    BlockAnchor,
}

impl MatchStrategy {
    pub fn label(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::LineTrimmed => "whitespace-trimmed",
            Self::IndentationNormalized => "indentation-adjusted",
            Self::BlockAnchor => "block-anchor",
        }
    }
}

/// Result of applying one or more edits to a file's text.
#[derive(Debug, Clone)]
pub struct EditOutcome {
    pub updated: String,
    pub replacements: usize,
    pub strategy: MatchStrategy,
    pub diff: String,
}

/// Applies a single edit to `text` and computes a concise unified diff for `display_path`.
pub fn apply_edit(
    display_path: &str,
    text: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Result<EditOutcome, String> {
    let (updated, replacements, strategy) = replace_one(display_path, text, old_string, new_string, replace_all)?;
    let diff = unified_diff(display_path, text, &updated, 3, 120);
    Ok(EditOutcome { updated, replacements, strategy, diff })
}

/// Applies multiple edits atomically (each against the result of the previous one).
/// If any edit fails, no partial result is returned.
pub fn apply_multi_edit(display_path: &str, text: &str, specs: &[EditSpec]) -> Result<EditOutcome, String> {
    if specs.is_empty() {
        return Err("`edits` is empty".into());
    }
    let mut current = text.to_string();
    let mut total = 0usize;
    let mut last_strategy = MatchStrategy::Exact;
    for (idx, spec) in specs.iter().enumerate() {
        let (next, count, strat) =
            replace_one(display_path, &current, &spec.old_string, &spec.new_string, spec.replace_all)
                .map_err(|e| format!("edit #{}: {e}", idx + 1))?;
        current = next;
        total += count;
        last_strategy = strat;
    }
    let diff = unified_diff(display_path, text, &current, 3, 160);
    Ok(EditOutcome { updated: current, replacements: total, strategy: last_strategy, diff })
}

fn replace_one(
    display_path: &str,
    text: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<(String, usize, MatchStrategy), String> {
    if old.is_empty() {
        return Err("old_string is empty; use write to create a file".into());
    }
    if old == new {
        return Err("old_string and new_string are the same".into());
    }

    // Normalize CRLF in old/new if the target file uses LF (and vice versa).
    let file_uses_crlf = text.contains("\r\n");
    let old_norm = normalize_eol(old, file_uses_crlf);
    let new_norm = normalize_eol(new, file_uses_crlf);

    // 1. Exact substring match.
    let exact_count = text.matches(&old_norm).count();
    if exact_count > 0 {
        if exact_count > 1 && !replace_all {
            return Err(format!(
                "old_string appears {exact_count} times in {display_path}; add surrounding lines to pick one, or set replace_all"
            ));
        }
        let updated = if replace_all {
            text.replace(&old_norm, &new_norm)
        } else {
            text.replacen(&old_norm, &new_norm, 1)
        };
        let count = if replace_all { exact_count } else { 1 };
        return Ok((updated, count, MatchStrategy::Exact));
    }

    // Line-based fallback strategies require line spans.
    let line_spans = split_line_spans(text);
    let old_lines: Vec<&str> = old_norm.lines().collect();
    if old_lines.is_empty() || line_spans.is_empty() {
        return Err(format!(
            "old_string isn't in {display_path}; read it again and copy the text exactly"
        ));
    }

    // 2. Indentation-normalized match (preserves target file indentation on replacement!).
    let indent_matches = find_indentation_matches(text, &line_spans, &old_lines);
    if !indent_matches.is_empty() {
        if indent_matches.len() > 1 && !replace_all {
            return Err(format!(
                "old_string matched {} blocks (ignoring indentation) in {display_path}; add surrounding lines to pick one, or set replace_all",
                indent_matches.len()
            ));
        }
        let target_matches = if replace_all { &indent_matches[..] } else { &indent_matches[..1] };
        let updated = apply_indented_spans(text, target_matches, &old_norm, &new_norm, file_uses_crlf);
        return Ok((updated, target_matches.len(), MatchStrategy::IndentationNormalized));
    }

    // 3. Line-trimmed match (handles trailing spaces or inconsistent leading spaces).
    let trimmed_matches = find_trimmed_matches(text, &line_spans, &old_lines);
    if !trimmed_matches.is_empty() {
        if trimmed_matches.len() > 1 && !replace_all {
            return Err(format!(
                "old_string matched {} places (ignoring whitespace) in {display_path}; add surrounding lines to pick one, or set replace_all",
                trimmed_matches.len()
            ));
        }
        let target_matches = if replace_all { &trimmed_matches[..] } else { &trimmed_matches[..1] };
        let updated = apply_indented_spans(text, target_matches, &old_norm, &new_norm, file_uses_crlf);
        return Ok((updated, target_matches.len(), MatchStrategy::LineTrimmed));
    }

    // 4. Block-anchor match (only for unique multi-line blocks with ≥3 lines and strong interior overlap).
    if !replace_all
        && old_lines.len() >= 3
        && let Some(span) = find_unique_block_anchor(text, &line_spans, &old_lines)
    {
        let updated = apply_indented_spans(text, &[span], &old_norm, &new_norm, file_uses_crlf);
        return Ok((updated, 1, MatchStrategy::BlockAnchor));
    }

    // Miss: provide a targeted closest-snippet hint so the model doesn't need to re-read the whole file.
    if let Some(hint) = closest_snippet_hint(text, &line_spans, &old_lines) {
        Err(format!(
            "old_string isn't in {display_path}; check exact text and retry.\n{hint}"
        ))
    } else {
        Err(format!(
            "old_string isn't in {display_path}; read it again and copy the text exactly"
        ))
    }
}

fn normalize_eol(s: &str, crlf: bool) -> String {
    let lf = s.replace("\r\n", "\n");
    if crlf { lf.replace('\n', "\r\n") } else { lf }
}

/// Byte span of one line in `text`: `(start, content_end)`.
#[derive(Debug, Clone, Copy)]
struct LineSpan {
    start: usize,
    content_end: usize,
}

fn split_line_spans(text: &str) -> Vec<LineSpan> {
    let mut spans = Vec::new();
    let bytes = text.as_bytes();
    let mut pos = 0;
    while pos < bytes.len() {
        let start = pos;
        while pos < bytes.len() && bytes[pos] != b'\n' {
            pos += 1;
        }
        let mut content_end = pos;
        if content_end > start && bytes[content_end - 1] == b'\r' {
            content_end -= 1;
        }
        let full_end = if pos < bytes.len() && bytes[pos] == b'\n' { pos + 1 } else { pos };
        spans.push(LineSpan { start, content_end });
        pos = full_end;
    }
    spans
}

#[derive(Debug, Clone, Copy)]
struct MatchedSpan {
    byte_start: usize,
    byte_end: usize,
    first_line_indent_len: usize,
    /// Common leading indent prefix from the matched block in the file.
    first_line_start: usize,
}

fn common_indent<'a>(lines: impl Iterator<Item = &'a str>) -> &'a str {
    let mut best: Option<&'a str> = None;
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let ws_len = line.len() - line.trim_start_matches([' ', '\t']).len();
        let ws = &line[..ws_len];
        best = Some(match best {
            None => ws,
            Some(prev) => common_prefix(prev, ws),
        });
    }
    best.unwrap_or("")
}

fn common_prefix<'a>(a: &'a str, b: &str) -> &'a str {
    let len = a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
    &a[..len]
}

fn strip_indent<'a>(line: &'a str, indent: &str) -> &'a str {
    line.strip_prefix(indent).unwrap_or(line)
}

fn find_indentation_matches(text: &str, spans: &[LineSpan], old_lines: &[&str]) -> Vec<MatchedSpan> {
    let n = old_lines.len();
    if n == 0 || spans.len() < n {
        return Vec::new();
    }
    let old_indent = common_indent(old_lines.iter().copied());
    let old_dedented: Vec<&str> = old_lines.iter().map(|l| strip_indent(l, old_indent).trim_end()).collect();

    let mut out = Vec::new();
    let mut i = 0;
    while i + n <= spans.len() {
        let window_lines: Vec<&str> = (i..i + n)
            .map(|k| &text[spans[k].start..spans[k].content_end])
            .collect();
        let file_indent = common_indent(window_lines.iter().copied());
        let matches = window_lines
            .iter()
            .zip(&old_dedented)
            .all(|(wl, ol)| strip_indent(wl, file_indent).trim_end() == *ol);
        if matches {
            out.push(MatchedSpan {
                byte_start: spans[i].start,
                byte_end: spans[i + n - 1].content_end,
                first_line_indent_len: file_indent.len(),
                first_line_start: spans[i].start,
            });
            i += n;
        } else {
            i += 1;
        }
    }
    out
}

fn find_trimmed_matches(text: &str, spans: &[LineSpan], old_lines: &[&str]) -> Vec<MatchedSpan> {
    let n = old_lines.len();
    if n == 0 || spans.len() < n {
        return Vec::new();
    }
    let old_trimmed: Vec<&str> = old_lines.iter().map(|l| l.trim()).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + n <= spans.len() {
        let ok = (0..n).all(|off| text[spans[i + off].start..spans[i + off].content_end].trim() == old_trimmed[off]);
        if ok {
            let window_lines = (i..i + n).map(|k| &text[spans[k].start..spans[k].content_end]);
            let file_indent = common_indent(window_lines);
            out.push(MatchedSpan {
                byte_start: spans[i].start,
                byte_end: spans[i + n - 1].content_end,
                first_line_indent_len: file_indent.len(),
                first_line_start: spans[i].start,
            });
            i += n;
        } else {
            i += 1;
        }
    }
    out
}

fn find_unique_block_anchor(text: &str, spans: &[LineSpan], old_lines: &[&str]) -> Option<MatchedSpan> {
    let n = old_lines.len();
    if n < 3 || spans.len() < n {
        return None;
    }
    let first_target = old_lines.first()?.trim();
    let last_target = old_lines.last()?.trim();
    if first_target.is_empty() || last_target.is_empty() {
        return None;
    }

    let mut candidates: Vec<(usize, f32, MatchedSpan)> = Vec::new();
    for i in 0..=spans.len() - n {
        let first = text[spans[i].start..spans[i].content_end].trim();
        let last = text[spans[i + n - 1].start..spans[i + n - 1].content_end].trim();
        if first != first_target || last != last_target {
            continue;
        }
        let interior = n - 2;
        let matching_interior = (1..n - 1)
            .filter(|&off| {
                let a = text[spans[i + off].start..spans[i + off].content_end].trim();
                let b = old_lines[off].trim();
                a == b || token_similarity(a, b) >= 0.75
            })
            .count();
        let ratio = matching_interior as f32 / interior as f32;
        if ratio >= 0.65 {
            let window_lines = (i..i + n).map(|k| &text[spans[k].start..spans[k].content_end]);
            let file_indent = common_indent(window_lines);
            candidates.push((
                i,
                ratio,
                MatchedSpan {
                    byte_start: spans[i].start,
                    byte_end: spans[i + n - 1].content_end,
                    first_line_indent_len: file_indent.len(),
                    first_line_start: spans[i].start,
                },
            ));
        }
    }

    if candidates.len() == 1 {
        Some(candidates[0].2)
    } else {
        None
    }
}

fn apply_indented_spans(
    text: &str,
    matches: &[MatchedSpan],
    old_norm: &str,
    new_norm: &str,
    file_uses_crlf: bool,
) -> String {
    let eol = if file_uses_crlf { "\r\n" } else { "\n" };
    let old_indent = common_indent(old_norm.lines());
    let new_lines: Vec<&str> = new_norm.lines().collect();
    let new_has_trailing_nl = new_norm.ends_with('\n');
    let old_has_trailing_nl = old_norm.ends_with('\n');

    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for m in matches {
        out.push_str(&text[cursor..m.byte_start]);
        // Re-indent `new_norm` from `old_indent` to the file's matched common indent.
        let file_indent = &text[m.first_line_start..m.first_line_start + m.first_line_indent_len];
        let mut rendered = Vec::with_capacity(new_lines.len());
        for line in &new_lines {
            if line.trim().is_empty() {
                rendered.push(String::new());
            } else {
                let dedented = strip_indent(line, old_indent);
                rendered.push(format!("{file_indent}{dedented}"));
            }
        }
        out.push_str(&rendered.join(eol));
        if new_has_trailing_nl && !old_has_trailing_nl {
            out.push_str(eol);
        }
        cursor = m.byte_end;
    }
    out.push_str(&text[cursor..]);
    out
}

fn token_similarity(a: &str, b: &str) -> f32 {
    if a == b {
        return 1.0;
    }
    let a_toks: Vec<&str> = a.split_whitespace().collect();
    let b_toks: Vec<&str> = b.split_whitespace().collect();
    if a_toks.is_empty() || b_toks.is_empty() {
        return 0.0;
    }
    let common = a_toks.iter().filter(|t| b_toks.contains(t)).count();
    (2.0 * common as f32) / (a_toks.len() + b_toks.len()) as f32
}

/// Finds the closest line window in `text` to `old_lines` to give the model a concise hint.
fn closest_snippet_hint(text: &str, spans: &[LineSpan], old_lines: &[&str]) -> Option<String> {
    let non_empty_old: Vec<&str> = old_lines.iter().map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
    if non_empty_old.is_empty() || spans.is_empty() {
        return None;
    }
    let window = old_lines.len().clamp(1, 12).min(spans.len());
    let mut best_score = 0.0f32;
    let mut best_idx = 0usize;

    for i in 0..=spans.len() - window {
        let mut score = 0.0f32;
        for (off, target) in non_empty_old.iter().take(window).enumerate() {
            let line = text[spans[i + off].start..spans[i + off].content_end].trim();
            score += token_similarity(line, target);
        }
        let avg = score / non_empty_old.len().min(window) as f32;
        if avg > best_score {
            best_score = avg;
            best_idx = i;
        }
    }

    if best_score < 0.35 {
        return None;
    }
    let start_line = best_idx.saturating_sub(1);
    let end_line = (best_idx + window + 1).min(spans.len());
    let mut hint = format!("Closest lines ({}–{}):\n", start_line + 1, end_line);
    for (idx, span) in spans[start_line..end_line].iter().enumerate() {
        let line_no = start_line + idx + 1;
        hint.push_str(&format!("{line_no:>6}\t{}\n", &text[span.start..span.content_end]));
    }
    Some(hint)
}

/// Generates a concise unified diff between `before` and `after`, capped at `max_lines`.
pub fn unified_diff(path: &str, before: &str, after: &str, context: usize, max_lines: usize) -> String {
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    if a == b {
        return String::new();
    }

    // Trim shared prefix and suffix so LCS stays small and fast even on large files.
    let mut prefix = 0;
    while prefix < a.len() && prefix < b.len() && a[prefix] == b[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < (a.len() - prefix)
        && suffix < (b.len() - prefix)
        && a[a.len() - 1 - suffix] == b[b.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let a_mid = &a[prefix..a.len() - suffix];
    let b_mid = &b[prefix..b.len() - suffix];

    #[derive(Clone, Copy)]
    enum Op {
        Equal(usize, usize),
        Delete(usize),
        Insert(usize),
    }

    let mut ops: Vec<Op> = Vec::new();
    for i in 0..prefix {
        ops.push(Op::Equal(i, i));
    }

    // Use LCS when the changed region is reasonably sized; otherwise emit a direct replace block.
    if a_mid.len().saturating_mul(b_mid.len()) <= 200_000 {
        let m = a_mid.len();
        let n = b_mid.len();
        let mut dp = vec![0u32; (m + 1) * (n + 1)];
        let idx = |i: usize, j: usize| i * (n + 1) + j;
        for i in (0..m).rev() {
            for j in (0..n).rev() {
                dp[idx(i, j)] = if a_mid[i] == b_mid[j] {
                    dp[idx(i + 1, j + 1)] + 1
                } else {
                    dp[idx(i + 1, j)].max(dp[idx(i, j + 1)])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < m && j < n {
            if a_mid[i] == b_mid[j] {
                ops.push(Op::Equal(prefix + i, prefix + j));
                i += 1;
                j += 1;
            } else if dp[idx(i + 1, j)] >= dp[idx(i, j + 1)] {
                ops.push(Op::Delete(prefix + i));
                i += 1;
            } else {
                ops.push(Op::Insert(prefix + j));
                j += 1;
            }
        }
        while i < m {
            ops.push(Op::Delete(prefix + i));
            i += 1;
        }
        while j < n {
            ops.push(Op::Insert(prefix + j));
            j += 1;
        }
    } else {
        for i in 0..a_mid.len() {
            ops.push(Op::Delete(prefix + i));
        }
        for j in 0..b_mid.len() {
            ops.push(Op::Insert(prefix + j));
        }
    }

    for k in 0..suffix {
        ops.push(Op::Equal(a.len() - suffix + k, b.len() - suffix + k));
    }

    // Group non-Equal ops into hunks with `context` surrounding lines.
    let changed: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter_map(|(i, op)| (!matches!(op, Op::Equal(_, _))).then_some(i))
        .collect();
    if changed.is_empty() {
        return String::new();
    }

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for pos in changed {
        let start = pos.saturating_sub(context);
        let end = (pos + context + 1).min(ops.len());
        if let Some(last) = ranges.last_mut()
            && start <= last.1
        {
            last.1 = last.1.max(end);
        } else {
            ranges.push((start, end));
        }
    }

    let mut out_lines = vec![format!("--- {path}"), format!("+++ {path}")];
    for (start, end) in ranges {
        let slice = &ops[start..end];
        let a_start = slice
            .iter()
            .find_map(|op| match *op {
                Op::Equal(ai, _) | Op::Delete(ai) => Some(ai + 1),
                Op::Insert(_) => None,
            })
            .unwrap_or(1);
        let b_start = slice
            .iter()
            .find_map(|op| match *op {
                Op::Equal(_, bi) | Op::Insert(bi) => Some(bi + 1),
                Op::Delete(_) => None,
            })
            .unwrap_or(1);
        let a_count = slice.iter().filter(|op| !matches!(op, Op::Insert(_))).count();
        let b_count = slice.iter().filter(|op| !matches!(op, Op::Delete(_))).count();
        out_lines.push(format!("@@ -{a_start},{a_count} +{b_start},{b_count} @@"));
        for op in slice {
            match *op {
                Op::Equal(ai, _) => out_lines.push(format!(" {}", a[ai])),
                Op::Delete(ai) => out_lines.push(format!("-{}", a[ai])),
                Op::Insert(bi) => out_lines.push(format!("+{}", b[bi])),
            }
            if out_lines.len() >= max_lines {
                out_lines.push("… [diff truncated]".into());
                return out_lines.join("\n");
            }
        }
    }
    out_lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_edit_produces_unified_diff() {
        let src = "fn one() {}\nfn two() {}\nfn three() {}\n";
        let out = apply_edit("src/lib.rs", src, "fn two() {}", "fn second() {}", false).unwrap();
        assert_eq!(out.strategy, MatchStrategy::Exact);
        assert_eq!(out.replacements, 1);
        assert!(out.updated.contains("fn second() {}"));
        assert!(out.diff.contains("-fn two() {}"));
        assert!(out.diff.contains("+fn second() {}"));
    }

    #[test]
    fn indentation_normalized_edit_preserves_file_indentation() {
        let src = "mod inner {\n    fn hello() {\n        println!(\"hi\");\n    }\n}\n";
        // Model omits the 4-space outer indent.
        let old = "fn hello() {\n    println!(\"hi\");\n}";
        let new = "fn hello() {\n    println!(\"hello world\");\n}";
        let out = apply_edit("src/lib.rs", src, old, new, false).unwrap();
        assert_eq!(out.strategy, MatchStrategy::IndentationNormalized);
        assert_eq!(
            out.updated,
            "mod inner {\n    fn hello() {\n        println!(\"hello world\");\n    }\n}\n"
        );
    }

    #[test]
    fn block_anchor_recovers_small_middle_typo() {
        let src = "fn compute(x: i32) -> i32 {\n    let a = x + 1;\n    let b = a * 2;\n    b\n}\n";
        let old = "fn compute(x: i32) -> i32 {\n    let a = x + 1;\n    let b = a * 3;\n    b\n}";
        let new = "fn compute(x: i32) -> i32 {\n    (x + 1) * 2\n}";
        let out = apply_edit("src/lib.rs", src, old, new, false).unwrap();
        assert_eq!(out.strategy, MatchStrategy::BlockAnchor);
        assert!(out.updated.contains("(x + 1) * 2"));
    }

    #[test]
    fn multi_edit_is_atomic_on_error() {
        let src = "let a = 1;\nlet b = 2;\n";
        let specs = vec![
            EditSpec { old_string: "let a = 1;".into(), new_string: "let a = 10;".into(), replace_all: false },
            EditSpec { old_string: "let z = 99;".into(), new_string: "let z = 0;".into(), replace_all: false },
        ];
        let err = apply_multi_edit("src/lib.rs", src, &specs).unwrap_err();
        assert!(err.contains("edit #2"), "{err}");
    }

    #[test]
    fn miss_returns_closest_snippet_hint() {
        let src = "fn alpha() {\n    let count = 42;\n    println!(\"{count}\");\n}\n";
        let err = apply_edit("src/lib.rs", src, "let count = 99;\nprintln!(\"{count}\");", "let count = 0;", false)
            .unwrap_err();
        assert!(err.contains("Closest lines"), "{err}");
        assert!(err.contains("let count = 42;"), "{err}");
    }
}
