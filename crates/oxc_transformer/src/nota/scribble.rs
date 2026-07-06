//! The Scribble whitespace algorithm (pure; unit-tested against the reference Scribble reader).

/// A body segment: a literal text run, or a whitespace-opaque element of type `E` (elements move
/// through the algorithm untouched).
pub(super) enum Seg<'a, E> {
    Text(&'a str),
    Elem(E),
}

/// A final body child: processed text, or an element passed through.
pub(super) enum Child<E> {
    Text(String),
    Elem(E),
}

/// One piece within a logical body line.
enum Piece<'a, E> {
    Text(&'a str),
    Elem(E),
}

/// Lower body segments to final children via the Scribble whitespace algorithm:
/// 1. Split into logical lines (`\n` in text splits lines; elements are non-whitespace content).
/// 2. Whitespace-only body: no newline → `[]`; else one `"\n"` per newline (brace bodies only —
///    a non-brace body has no `{`/`}` edges and is simply empty).
/// 3. Drop the single newline right after `{` and before `}` (brace body); a non-brace body
///    (document / colon / heading / list-item / control branch) trims *all* edge blank lines, and
///    its first line participates in indent stripping.
/// 4. Strip the common indentation of the indent lines (leftover indent stays joined to content,
///    per notation.md §Whitespace); trim each line's trailing whitespace (a brace body keeps the
///    `}`-line's).
/// 5. Emit one `"\n"` per inter-line newline, never coalesced — a blank line surfaces as two
///    adjacent `"\n"` children, the runtime's paragraph-break marker (decode.md §struct).
pub(super) fn lower<E>(segs: Vec<Seg<'_, E>>, is_brace: bool) -> Vec<Child<E>> {
    // --- Step 1: split into lines of pieces, counting newlines. ---
    let mut lines: Vec<Vec<Piece<E>>> = vec![Vec::new()];
    let mut newline_count = 0usize;
    let mut has_nonws = false;
    for seg in segs {
        match seg {
            Seg::Text(text) => {
                for (k, part) in text.split('\n').enumerate() {
                    if k > 0 {
                        lines.push(Vec::new());
                        newline_count += 1;
                    }
                    // CRLF: a `\r` right before the split `\n` is part of the line terminator.
                    let part = part.strip_suffix('\r').unwrap_or(part);
                    // A bare CR and the Unicode line/paragraph separators are line breaks too.
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
            Seg::Elem(e) => {
                has_nonws = true;
                lines.last_mut().unwrap().push(Piece::Elem(e));
            }
        }
    }

    // --- Step 2: whitespace-only body. ---
    if !has_nonws {
        if !is_brace || newline_count == 0 {
            return Vec::new(); // `@p{}` / `@p{   }` → []
        }
        return std::iter::repeat_with(|| Child::Text("\n".to_string()))
            .take(newline_count)
            .collect();
    }

    // --- Step 3: trim edges. `first_line_is_indent` records whether the first remaining line
    // participates in common-indent stripping (the `{`-line's leading space is content). ---
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
    let indent_from = usize::from(!first_line_is_indent);
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
    for (i, line) in lines.into_iter().enumerate() {
        if i > 0 {
            out.push(Child::Text("\n".to_string()));
        }
        let is_indent_line = i >= indent_from;
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

/// Emit one line: merge consecutive text pieces into runs (balanced braces arrive split —
/// `@code{f{o}o}` must surface as one `"f{o}o"` child), strip `strip` bytes of common indent off
/// an indent line's first run (leftover indent stays joined to the content), and trim the final
/// run's trailing whitespace unless `keep_trailing`.
fn emit_line<E>(
    out: &mut Vec<Child<E>>,
    line: Vec<Piece<'_, E>>,
    strip: usize,
    is_indent_line: bool,
    keep_trailing: bool,
) {
    enum Run<E> {
        Text(String),
        Elem(E),
    }
    let mut runs: Vec<Run<E>> = Vec::new();
    for piece in line {
        match piece {
            Piece::Text(s) => {
                if let Some(Run::Text(buf)) = runs.last_mut() {
                    buf.push_str(s);
                } else {
                    runs.push(Run::Text(s.to_string()));
                }
            }
            Piece::Elem(e) => runs.push(Run::Elem(e)),
        }
    }
    if runs.is_empty() {
        return;
    }
    let last = runs.len() - 1;

    for (i, run) in runs.into_iter().enumerate() {
        match run {
            Run::Text(mut s) => {
                if i == 0 && is_indent_line {
                    let lead = s.bytes().take_while(|b| *b == b' ' || *b == b'\t').count();
                    let drop = strip.min(lead);
                    s = s[drop..].to_string();
                }
                if i == last && !keep_trailing {
                    let trimmed = s.trim_end_matches([' ', '\t']);
                    s.truncate(trimmed.len());
                }
                if !s.is_empty() {
                    out.push(Child::Text(s));
                }
            }
            Run::Elem(e) => out.push(Child::Elem(e)),
        }
    }
}

/// True if a line has no content (no elements; all text whitespace/empty).
fn line_is_blank<E>(line: &[Piece<'_, E>]) -> bool {
    line.iter().all(|p| match p {
        Piece::Text(s) => s.bytes().all(|b| b.is_ascii_whitespace()),
        Piece::Elem(_) => false,
    })
}

/// Leading whitespace length (bytes) before the first content piece of a line.
fn leading_ws_len<E>(line: &[Piece<'_, E>]) -> usize {
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
    use super::{Child, Seg, lower};

    /// Render the algorithm's output in Scribble's notation: text children quoted, elements as `E`.
    fn render(segs: Vec<Seg<'_, u32>>) -> String {
        let parts: Vec<String> = lower(segs, true)
            .into_iter()
            .map(|child| match child {
                Child::Text(s) => format!("{s:?}"),
                Child::Elem(_) => "E".to_string(),
            })
            .collect();
        parts.join(" ")
    }

    const E: Seg<'static, u32> = Seg::Elem(0);
    fn t(s: &str) -> Seg<'_, u32> {
        Seg::Text(s)
    }

    #[test]
    fn empty_and_ws_only() {
        assert_eq!(render(vec![]), "");
        assert_eq!(render(vec![t("   ")]), ""); // ws-only, no newline → []
        assert_eq!(render(vec![t("\n")]), r#""\n""#); // only newline → one "\n"
        assert_eq!(render(vec![t("\n\n\n")]), r#""\n" "\n" "\n""#);
    }

    #[test]
    fn single_line_spaces_kept() {
        assert_eq!(render(vec![t(" bar ")]), r#"" bar ""#);
    }

    #[test]
    fn drop_open_close_newline_strip_indent() {
        // `@foo{⏎  bar⏎}` → "bar"
        assert_eq!(render(vec![t("\n  bar\n")]), r#""bar""#);
        // `@foo{⏎  begin⏎    x⏎  end}` → "begin","⏎","  x","⏎","end" (kept indent joined)
        assert_eq!(render(vec![t("\n  begin\n    x\n  end")]), r#""begin" "\n" "  x" "\n" "end""#);
    }

    #[test]
    fn crlf_is_normalized() {
        // `\r\n` line endings: the `\r` is part of the terminator, dropped.
        assert_eq!(render(vec![t("line1\r\nline2")]), r#""line1" "\n" "line2""#);
        // a trailing `\r` (e.g. the `\r\n` after a closing brace) is not a stray node.
        assert_eq!(render(vec![t("\r\n")]), r#""\n""#);
    }

    #[test]
    fn blank_line_is_two_newlines() {
        assert_eq!(render(vec![t("\n  bar\n\n  baz\n")]), r#""bar" "\n" "\n" "baz""#);
        assert_eq!(render(vec![t("\n\n  bar\n\n")]), r#""\n" "bar" "\n""#);
    }

    #[test]
    fn common_indent_keeps_leftover() {
        assert_eq!(
            render(vec![t("bar\n       baz\n     bbb")]),
            r#""bar" "\n" "  baz" "\n" "bbb""#
        );
    }

    #[test]
    fn balanced_braces_merge_to_one_text() {
        // `@code{f{o}o}` → "f{o}o" (balanced braces arrive as separate Text segs, must merge).
        assert_eq!(render(vec![t("f"), t("{"), t("o"), t("}"), t("o")]), r#""f{o}o""#);
    }

    #[test]
    fn element_keeps_preceding_space() {
        // `@foo{bar @baz …⏎     blah}` line: "bar " <E>; trailing space before E kept.
        assert_eq!(render(vec![t("bar "), E, t("\n     blah")]), r#""bar " E "\n" "blah""#);
    }
}
