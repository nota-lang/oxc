//! The Scribble whitespace algorithm (pure; unit-tested against the reference Scribble
//! reader). Moved verbatim from the parser's reader.

/// A body segment fed to the whitespace algorithm. Both variants are `Copy` (no `Expression`
/// is held here — elements are referenced by index), so lines can be re-walked without clones.
#[derive(Clone, Copy)]
pub(super) enum Seg<'a> {
    /// A literal text run (raw source slice).
    Text(&'a str),
    /// A (whitespace-opaque) element, by index into the caller's element vec.
    Elem(usize),
}

/// A final body child spec: processed text, or an element index (the caller moves it out).
pub(super) enum ChildSpec {
    Text(String),
    Elem(usize),
}

/// One piece within a logical body line.
#[derive(Clone, Copy)]
enum Piece<'a> {
    Text(&'a str),
    Elem(usize),
}

/// Lower body segments to final child specs via the Scribble algorithm (verified against
/// Scribble's own reader):
/// 1. Split into logical lines (`\n` in text splits lines; elements are non-ws content).
/// 2. Whitespace-only body: no newline → `[]`; else → one `"\n"` per newline.
/// 3. Drop the single newline right after `{` (leading all-ws line) and before `}` (trailing
///    all-ws line) — *unless* the body is only newlines (step 2).
/// 4. Strip the common indentation of the indent lines, keeping the leftover indent as its own
///    text child; trim each interior line's trailing whitespace (keep the `}`-line's).
/// 5. Emit one `"\n"` per inter-line newline — never coalesced.
pub(super) fn lower<'a>(segs: &[Seg<'a>], is_brace: bool) -> Vec<ChildSpec> {
    // --- Step 1: split into lines of pieces, counting newlines. ---
    let mut lines: Vec<Vec<Piece<'a>>> = vec![Vec::new()];
    let mut newline_count = 0usize;
    let mut has_nonws = false;
    for seg in segs {
        match *seg {
            Seg::Text(text) => {
                for (k, part) in text.split('\n').enumerate() {
                    if k > 0 {
                        lines.push(Vec::new());
                        newline_count += 1;
                    }
                    // Normalize CRLF: a `\r` right before the split `\n` is part of the line
                    // terminator, not content — drop one trailing `\r` so Windows-authored files
                    // don't leak stray carriage returns into text (and a trailing `\r\n` after a
                    // closing `}` doesn't surface as a stray `"\r"` sibling).
                    let part = part.strip_suffix('\r').unwrap_or(part);
                    // A bare CR (old-Mac line ending) and the Unicode line/paragraph separators
                    // (U+2028/U+2029) are line breaks too — split on them so they don't survive as
                    // literal characters in the text.
                    for (m, sub) in part.split(['\r', '\u{2028}', '\u{2029}']).enumerate() {
                        if m > 0 {
                            lines.push(Vec::new());
                            newline_count += 1;
                        }
                        if !sub.is_empty() {
                            if sub.bytes().any(|b| !b.is_ascii_whitespace()) {
                                has_nonws = true;
                            }
                            lines.last_mut().unwrap().push(Piece::Text(sub));
                        }
                    }
                }
            }
            Seg::Elem(idx) => {
                has_nonws = true;
                lines.last_mut().unwrap().push(Piece::Elem(idx));
            }
        }
    }

    // --- Step 2: whitespace-only body. ---
    if !has_nonws {
        // A non-brace body (document / heading / list-item / control-flow branch) has no `{`/`}`
        // edges, so a whitespace-only body is simply empty — no stray "\n" children.
        if !is_brace {
            return Vec::new();
        }
        if newline_count == 0 {
            return Vec::new(); // `@p{}` / `@p{   }` → []
        }
        return std::iter::repeat_with(|| ChildSpec::Text("\n".to_string()))
            .take(newline_count)
            .collect();
    }

    // --- Step 3: trim edges. ---
    // A brace body drops the single `{`-newline / `}`-newline (its surrounding spaces are content).
    // A non-brace body has no `{`/`}` edges, so it trims ALL leading/trailing whitespace-only lines
    // and its first line is a normal indent line. `first_line_is_indent` records whether the first
    // line participates in common-indent stripping.
    let mut first_line_is_indent = false;
    if is_brace {
        if lines.len() >= 2 && line_is_blank(&lines[0]) {
            lines.remove(0);
            first_line_is_indent = true;
        }
        if lines.len() >= 2 && line_is_blank(lines.last().unwrap()) {
            lines.pop();
        }
    } else {
        while lines.len() > 1 && line_is_blank(&lines[0]) {
            lines.remove(0);
        }
        while lines.len() > 1 && line_is_blank(lines.last().unwrap()) {
            lines.pop();
        }
        first_line_is_indent = true;
    }

    // --- Step 4: common indentation over the indent lines. ---
    let indent_from = usize::from(!first_line_is_indent); // skip the `{`-line if it is content
    let common = lines
        .iter()
        .enumerate()
        .filter(|(i, line)| *i >= indent_from && !line_is_blank(line))
        .map(|(_, line)| leading_ws_len(line))
        .min()
        .unwrap_or(0);

    // --- Step 5: emit. ---
    let mut out = Vec::new();
    let last_idx = lines.len() - 1;
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push(ChildSpec::Text("\n".to_string())); // one "\n" per inter-line newline
        }
        let is_indent_line = i >= indent_from;
        // Keep the final line's trailing whitespace only for a brace body (the `}`-line); a
        // non-brace body trims every line's trailing whitespace.
        let keep_trailing = is_brace && i == last_idx;
        emit_line(
            &mut out,
            line,
            if is_indent_line { common } else { 0 },
            is_indent_line,
            keep_trailing,
        );
    }
    out
}

/// Emit one line.
///
/// Consecutive text pieces are first merged into a single run (balanced braces split text into
/// `f`/`{`/`o`/… pieces — `@code{f{o}o}` must surface as one `"f{o}o"` child). Then:
/// * For an *indent line*: strip `strip` leading whitespace bytes (the common indent), emitting
///   any *leftover* indent (past the common amount) as its own text child, then the content. For
///   the `{`-line (not an indent line), leading whitespace is content (between `{` and text), kept.
/// * Trim the line-final text run's trailing whitespace unless `keep_trailing` (a brace `}`-line).
fn emit_line(
    out: &mut Vec<ChildSpec>,
    line: &[Piece<'_>],
    strip: usize,
    is_indent_line: bool,
    keep_trailing: bool,
) {
    // --- Merge consecutive text into runs. ---
    enum Run {
        Text(String),
        Elem(usize),
    }
    let mut runs: Vec<Run> = Vec::new();
    for piece in line {
        match *piece {
            Piece::Text(s) => {
                if let Some(Run::Text(buf)) = runs.last_mut() {
                    buf.push_str(s);
                } else {
                    runs.push(Run::Text(s.to_string()));
                }
            }
            Piece::Elem(idx) => runs.push(Run::Elem(idx)),
        }
    }
    if runs.is_empty() {
        return;
    }
    let last = runs.len() - 1;

    // --- Leading common-indent strip (indent lines only), emitting leftover indent separately. ---
    for (i, run) in runs.into_iter().enumerate() {
        match run {
            Run::Text(mut s) => {
                if i == 0 && is_indent_line {
                    // Strip the common indent; any leftover indent (past the common amount) stays
                    // JOINED with the line's content as one text child (e.g. "  x", not "  ","x").
                    let lead = s.bytes().take_while(|b| *b == b' ' || *b == b'\t').count();
                    let drop = strip.min(lead);
                    s = s[drop..].to_string();
                }
                // Trailing-trim the line-final text run unless the body keeps it (a brace `}`-line).
                if i == last && !keep_trailing {
                    let trimmed = s.trim_end_matches([' ', '\t']);
                    s.truncate(trimmed.len());
                }
                if !s.is_empty() {
                    out.push(ChildSpec::Text(s));
                }
            }
            Run::Elem(idx) => out.push(ChildSpec::Elem(idx)),
        }
    }
}

/// True if a line has no content (no elements; all text whitespace/empty).
fn line_is_blank(line: &[Piece]) -> bool {
    line.iter().all(|p| match p {
        Piece::Text(s) => s.bytes().all(|b| b.is_ascii_whitespace()),
        Piece::Elem(_) => false,
    })
}

/// Leading whitespace length (bytes) before the first content piece of a line.
fn leading_ws_len(line: &[Piece]) -> usize {
    let mut n = 0;
    for p in line {
        match p {
            Piece::Text(s) => {
                let lead = s.bytes().take_while(u8::is_ascii_whitespace).count();
                n += lead;
                if lead < s.len() {
                    return n;
                }
            }
            Piece::Elem(_) => return n,
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::{ChildSpec, Seg, lower};

    /// Render the whitespace algorithm's output in Scribble's `(foo …)` notation for assertion:
    /// text children quoted, element children as `E`. Mirrors Scribble's own reader output.
    fn render(segs: &[Seg]) -> String {
        let mut parts: Vec<String> = Vec::new();
        for child in lower(segs, true) {
            match child {
                ChildSpec::Text(s) => parts.push(format!("{s:?}")),
                ChildSpec::Elem(_) => parts.push("E".to_string()),
            }
        }
        parts.join(" ")
    }

    const E: Seg<'static> = Seg::Elem(0);
    fn t(s: &str) -> Seg<'_> {
        Seg::Text(s)
    }

    #[test]
    fn empty_and_ws_only() {
        assert_eq!(render(&[]), "");
        assert_eq!(render(&[t("   ")]), ""); // ws-only, no newline → []
        assert_eq!(render(&[t("\n")]), r#""\n""#); // only newline → one "\n"
        assert_eq!(render(&[t("\n\n\n")]), r#""\n" "\n" "\n""#);
    }

    #[test]
    fn single_line_spaces_kept() {
        assert_eq!(render(&[t(" bar ")]), r#"" bar ""#);
    }

    #[test]
    fn drop_open_close_newline_strip_indent() {
        // `@foo{⏎  bar⏎}` → "bar"
        assert_eq!(render(&[t("\n  bar\n")]), r#""bar""#);
        // `@foo{⏎  begin⏎    x⏎  end}` → "begin","⏎","  x","⏎","end" (kept indent joined to content)
        assert_eq!(render(&[t("\n  begin\n    x\n  end")]), r#""begin" "\n" "  x" "\n" "end""#);
    }

    #[test]
    fn crlf_is_normalized() {
        // `\r\n` line endings: the `\r` is part of the terminator, dropped → same shape as `\n`.
        assert_eq!(render(&[t("line1\r\nline2")]), r#""line1" "\n" "line2""#);
        // a trailing `\r` (e.g. the `\r\n` after a closing brace) is not emitted as a stray node.
        assert_eq!(render(&[t("\r\n")]), r#""\n""#);
    }

    #[test]
    fn blank_line_is_two_newlines() {
        // a blank line surfaces as ≥2 adjacent "\n".
        assert_eq!(render(&[t("\n  bar\n\n  baz\n")]), r#""bar" "\n" "\n" "baz""#);
        // leading + trailing blank → "⏎","bar","⏎"
        assert_eq!(render(&[t("\n\n  bar\n\n")]), r#""\n" "bar" "\n""#);
    }

    #[test]
    fn common_indent_keeps_leftover() {
        assert_eq!(render(&[t("bar\n       baz\n     bbb")]), r#""bar" "\n" "  baz" "\n" "bbb""#);
    }

    #[test]
    fn balanced_braces_merge_to_one_text() {
        // `@code{f{o}o}` → "f{o}o" (balanced braces arrive as separate Text segs, must merge).
        assert_eq!(render(&[t("f"), t("{"), t("o"), t("}"), t("o")]), r#""f{o}o""#);
    }

    #[test]
    fn element_keeps_preceding_space() {
        // `@foo{bar @baz …⏎     blah}` line: "bar " <E> ; trailing space before E kept.
        assert_eq!(render(&[t("bar "), E, t("\n     blah")]), r#""bar " E "\n" "blah""#);
    }
}
