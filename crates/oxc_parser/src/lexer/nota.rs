//! Lexer scan-methods for Nota `@`-markup.
//!
//! Nota is markup-outer, JS-embedded (the inverse of JSX). [`Lexer::next_nota_child`] is the markup
//! analog of [`super::jsx`]'s `next_jsx_child` (and of Typst's markup-mode lexer): the parser drives
//! the lexer into "markup body" mode and pulls one child token at a time, dispatching on its
//! [`Kind`] — never peeking raw bytes. Each call returns either a maximal literal-text run
//! ([`Kind::MarkupText`]) or a single markup *sigil* as a typed token (`@`/`{`/`}`/`\n`/`*`/`_`/`\`/
//! `` ` ``/`$`/`|`). The token's value is read via `token_source` (never `cur_string`); escape,
//! emphasis word-boundary, raw-span extent, and whitespace (the Scribble algorithm) are owned by the
//! Nota parser layer (the `nota` module), which keeps embedded spans byte-identical with the source.
//!
//! Sigils are *consumed* (unlike JSX's `<`/`{`, which are also returned as kinds): the parser tracks
//! brace depth off the typed `LCurly`/`RCurly` tokens (a balanced `{…}` inside a body is literal
//! text — Scribble `@foo{f{o}o}` → `"f{o}o"`) and recurses into `@`-forms on `At`. The parser's
//! body collectors re-seek (`nota_seek_markup`) when a span helper (code/math/emphasis) consumes a
//! larger extent than one sigil.

// Source offsets and substring lengths are cast to `u32` throughout: oxc's `Span` is `u32`-based
// (sources are bounded to 4 GiB), so these `as u32` casts cannot truncate in practice.
#![expect(
    clippy::cast_possible_truncation,
    reason = "source offsets/lengths fit in u32 (oxc's Span model)"
)]

use oxc_span::Span;
use oxc_syntax::identifier::{is_identifier_part, is_identifier_start};

use super::{
    Kind, Lexer, Token,
    search::{SafeByteMatchTable, byte_search, safe_byte_match_table},
};
use crate::{
    config::LexerConfig as Config,
    nota::{ElsePeek, ListMarker, MarkupTrigger},
};

/// Bytes that terminate a Nota markup-text run — i.e. the markup *sigils* that
/// [`Lexer::next_nota_child`] returns as their own typed tokens: `}` (body close), `@` (`@`-form),
/// `{` (nested body), `\n` (line boundary — for line-start `%`/`%%%` statements, headings, lists,
/// and the Scribble per-line whitespace algorithm), `*`/`_` (emphasis — the parser applies the Typst
/// word-boundary rule to decide marker-vs-literal), `\` (general escape), and the raw-span openers
/// `` ` `` (inline/fenced code), `$` (math), and `|` (a literal `|`, or the `|{ … }|` verbatim body).
static MARKUP_TEXT_END_TABLE: SafeByteMatchTable = safe_byte_match_table!(|b| b == b'}'
    || b == b'@'
    || b == b'{'
    || b == b'\n'
    || b == b'*'
    || b == b'_'
    || b == b'\\'
    || b == b'`'
    || b == b'$'
    || b == b'|');

impl<C: Config> Lexer<'_, C> {
    /// Pull one Nota markup-body *child token* at the current source position (the markup analog of
    /// [`Self::next_jsx_child`]).
    ///
    /// Returns either a maximal literal-text run ([`Kind::MarkupText`], always ≥1 byte) or, when
    /// positioned on a markup sigil, that sigil *consumed* and returned as a typed token:
    /// `@`→[`Kind::At`], `{`→[`Kind::LCurly`], `}`→[`Kind::RCurly`], `\n`→[`Kind::NotaNewline`],
    /// `*`→[`Kind::Star`], `_`→[`Kind::NotaUnderscore`], `\`→[`Kind::NotaBackslash`],
    /// `` ` ``→[`Kind::NotaBacktick`], `$`→[`Kind::NotaDollar`], `|`→[`Kind::Pipe`]. At EOF returns
    /// [`Kind::Eof`]. The parser dispatches on the kind; for sigils whose semantics span more than
    /// one byte (code/math/emphasis/escape) the parser's helper re-scans from the token start and
    /// re-seeks. Produced via `finish_re_lex` so it never pollutes the externally-collected stream.
    pub(crate) fn next_nota_child(&mut self) -> Token {
        let start = self.offset();
        self.token.set_start(start);

        let kind = match self.peek_byte() {
            // Sigils: consume the single significant byte and return its typed kind.
            Some(b'@') => Kind::At,
            Some(b'{') => Kind::LCurly,
            Some(b'}') => Kind::RCurly,
            Some(b'\n') => Kind::NotaNewline,
            // Emphasis: a marker token only at a valid opener (the Typst word-boundary rule, owned by
            // the lexer — `'*' if !in_word()`). Otherwise the `*`/`_` is literal: emit it as a 1-byte
            // text token so the parser treats it as text without re-checking. (The matching *close* is
            // still resolved by the parser.)
            Some(b @ (b'*' | b'_')) => {
                let kind = if emphasis_can_open(self.source.whole(), start as usize, b) {
                    if b == b'*' { Kind::Star } else { Kind::NotaUnderscore }
                } else {
                    Kind::MarkupText
                };
                self.consume_char();
                return self.finish_re_lex(kind);
            }
            Some(b'\\') => Kind::NotaBackslash,
            Some(b'`') => Kind::NotaBacktick,
            Some(b'$') => Kind::NotaDollar,
            Some(b'|') => Kind::Pipe,
            // Literal text: maximal run up to (not including) the next sigil, mirroring
            // `read_jsx_child`'s `byte_search!`. The leading byte is non-sigil, so the run is ≥1 byte.
            Some(_) => {
                byte_search! {
                    lexer: self,
                    table: MARKUP_TEXT_END_TABLE,
                    handle_eof: {
                        return self.finish_re_lex(Kind::MarkupText);
                    },
                };
                return self.finish_re_lex(Kind::MarkupText);
            }
            None => return self.finish_re_lex(Kind::Eof),
        };
        // Every sigil byte matched above is ASCII, so this consumes exactly that one byte.
        self.consume_char();
        self.finish_re_lex(kind)
    }

    /// Lex a Nota `@`-form *head* identifier (Nota reader): `@foo`, `@if`, `@café`.
    ///
    /// Mirrors Typst's per-mode (code-mode) identifier lexing rather than JS identifier lexing: a
    /// `\` is **not** the start of a `\u` escape here — it (like any non-identifier char) simply
    /// *terminates* the head. That is what lets the Nota escape `@foo\:` lex as the head `foo`
    /// followed by the literal escape `\:`, instead of the JS identifier reader choking on a bad
    /// Unicode escape mid-head. Keyword heads route through [`Kind::match_keyword`], so `@if`/`@for`
    /// still produce [`Kind::If`]/[`Kind::For`] for the parser's control-flow dispatch.
    ///
    /// Entered positioned at the head's first char (just past `@`), which MUST be an
    /// identifier-start — the caller checks this and otherwise keeps the JS-lexed path (`@(expr)`,
    /// `@{…}`). Leaves the source positioned at the first non-identifier char. Produced via
    /// `finish_re_lex` (like [`Self::next_nota_child`]) so it never pollutes the token stream.
    pub(crate) fn next_nota_head(&mut self) -> Token {
        let start = self.source.position();
        debug_assert!(self.peek_char().is_some_and(is_identifier_start));
        while self.peek_char().is_some_and(is_identifier_part) {
            self.consume_char();
        }
        let kind = Kind::match_keyword(self.source.str_from_pos_to_current(start));
        self.finish_re_lex(kind)
    }
}

// ================================================================================================
// Nota lexical scans (read-only, offset-addressed). These own the byte-munging the Nota parser used
// to inline: the parser passes its `source_text` and gets back offsets / spans / `&str` / classified
// results, then builds the AST. Keeping them here (not in `nota/mod.rs`) keeps byte handling on the
// lexer side. Pure functions of `(source, offset)` — they never touch the lexer cursor.
// ================================================================================================

/// Scan a custom-element name tail (`@my-widget`, `@x-y-z`) at `at`.
///
/// One or more `-`-joined runs of identifier chars. Returns the offset past the tail, or `None` if
/// `at` is not a `-` directly followed by an identifier char. (The JS lexer stops a bare identifier at
/// `-`, so the tail is read here over the source bytes — the markup analog of JSX's
/// `continue_lex_jsx_identifier`.)
pub fn scan_hyphen_tail(source: &str, at: u32) -> Option<u32> {
    let bytes = source.as_bytes();
    let mut i = at as usize;
    let mut consumed = false;
    while bytes.get(i) == Some(&b'-')
        && bytes.get(i + 1).is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
    {
        i += 1; // the `-`
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        consumed = true;
    }
    consumed.then_some(i as u32)
}

/// Classify the byte at `after` as the trigger glued to a head: `{`/`[`/`:`/`|{` → element forms,
/// anything else → interpolation. (The one-byte head→body peek; see [`MarkupTrigger`].)
pub fn markup_trigger(source: &str, after: u32) -> MarkupTrigger {
    let bytes = source.as_bytes();
    match bytes.get(after as usize) {
        Some(b'{') => MarkupTrigger::Brace,
        Some(b'[') => MarkupTrigger::Bracket,
        Some(b':') => MarkupTrigger::Colon,
        // `|{` opens a verbatim body; a lone `|` is not a trigger.
        Some(b'|') if bytes.get(after as usize + 1) == Some(&b'{') => MarkupTrigger::Verbatim,
        _ => MarkupTrigger::None,
    }
}

/// Resolve a backslash escape at `esc_off`: `\<c>` → the literal `<c>` (a source slice; the `\` is
/// dropped), resuming past `\<c>`. A trailing lone `\` at EOF → literal `\`. Returns `(literal, resume)`.
pub fn escape_extent(source: &str, esc_off: u32) -> (&str, u32) {
    match source[esc_off as usize + 1..].chars().next() {
        Some(c) => {
            let start = esc_off as usize + 1;
            (&source[start..start + c.len_utf8()], esc_off + 1 + c.len_utf8() as u32)
        }
        // Trailing lone backslash at EOF: literal `\`.
        None => ("\\", esc_off + 1),
    }
}

/// Does `source[at..]` begin with the keyword `kw` followed by a word boundary (not an
/// identifier-continue char), so `else`/`if` match but `elsewhere`/`iffy` do not?
pub fn matches_keyword(source: &str, at: usize, kw: &[u8]) -> bool {
    let bytes = source.as_bytes();
    if at + kw.len() > bytes.len() || &bytes[at..at + kw.len()] != kw {
        return false;
    }
    match bytes.get(at + kw.len()) {
        Some(b) => !(b.is_ascii_alphanumeric() || matches!(b, b'_' | b'$') || *b >= 0x80),
        None => true,
    }
}

/// Scan for an `else`/`else if` continuation after an `@if` branch that closed at `close_end`.
///
/// Skip inline whitespace and up to one newline (a blank line breaks the chain), then match `else`
/// (rejecting an escaped `\else`) and classify what follows (`if` → else-if, `{` → else-block).
pub fn else_peek(source: &str, close_end: u32) -> ElsePeek {
    let bytes = source.as_bytes();
    let mut i = close_end as usize;
    let mut newlines = 0u32;
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b'\r' => i += 1,
            b'\n' => {
                newlines += 1;
                if newlines >= 2 {
                    return ElsePeek::None; // blank line: continuation broken
                }
                i += 1;
            }
            _ => break,
        }
    }
    // `\else` — an escaped literal, not a continuation.
    if i < bytes.len() && bytes[i] == b'\\' {
        return ElsePeek::None;
    }
    if !matches_keyword(source, i, b"else") {
        return ElsePeek::None;
    }
    // After `else`, skip whitespace and look for `if` (→ `else if`) or `{` (→ `else {`).
    let mut j = i + 4;
    while j < bytes.len() && matches!(bytes[j], b' ' | b'\t' | b'\r' | b'\n') {
        j += 1;
    }
    if matches_keyword(source, j, b"if") {
        ElsePeek::ElseIf { if_offset: j as u32 }
    } else if j < bytes.len() && bytes[j] == b'{' {
        ElsePeek::Else { brace_offset: j as u32 }
    } else {
        ElsePeek::None
    }
}

/// The source [`Span`] of a text run `t`, recovered from its subslice position within `source`.
///
/// A body text run is either a `&source[a..b]` slice of real content (markup text, an escaped char
/// that *is* in source) — which gets its true `a..b` span — or a synthesized/`'static` literal
/// (`"\n"`/`"{"`/`"}"`/`"|"`) foreign to the source allocation, which gets an empty `0..0` span (it
/// is punctuation/whitespace, never a navigation target). The test is a plain pointer-offset compare.
pub fn span_of_slice(source: &str, t: &str) -> Span {
    let base = source.as_ptr() as usize;
    let lo = t.as_ptr() as usize;
    let hi = lo + t.len();
    if lo >= base && hi <= base + source.len() {
        Span::new((lo - base) as u32, (hi - base) as u32)
    } else {
        Span::empty(0)
    }
}

/// Offset just past the next `\n` at/after `offset` (or EOF if none) — the start of the next line.
pub fn next_line_start(source: &str, offset: u32) -> u32 {
    let bytes = source.as_bytes();
    let mut i = offset as usize;
    while i < bytes.len() && bytes[i] != b'\n' {
        i += 1;
    }
    if i < bytes.len() { i as u32 + 1 } else { i as u32 }
}

/// Peek the raw byte at `offset`, or `None` at/after end of source.
pub fn byte_at(source: &str, offset: u32) -> Option<u8> {
    source.as_bytes().get(offset as usize).copied()
}

/// Is the char at `off` an identifier-start? Used to decide a Nota `@`-head (bare ident → lex with
/// Nota rules) from a JS-lexed head (`@(expr)`/`@{…}`).
pub fn is_ident_start_at(source: &str, off: u32) -> bool {
    source[off as usize..].chars().next().is_some_and(is_identifier_start)
}

/// If the line at `line_start` is a colon-sugar `|`-prop line (first non-whitespace is `|`), return
/// the offset just past the `|` (where the `[k:v]`-style entries begin); else `None`.
pub fn colon_prop_line_at(source: &str, line_start: u32) -> Option<u32> {
    let bytes = source.as_bytes();
    let mut i = line_start as usize;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    (bytes.get(i) == Some(&b'|')).then_some(i as u32 + 1)
}

/// The offset of the terminating `\n` of the line containing `line_start` (or EOF if none).
pub fn line_content_end(source: &str, line_start: u32) -> u32 {
    let bytes = source.as_bytes();
    let mut i = line_start as usize;
    while i < bytes.len() && bytes[i] != b'\n' {
        i += 1;
    }
    i as u32
}

/// The indentation (leading-space count) of the line containing byte `offset`.
pub fn line_indent_of(source: &str, offset: u32) -> usize {
    let bytes = source.as_bytes();
    let mut start = offset as usize;
    while start > 0 && bytes[start - 1] != b'\n' {
        start -= 1;
    }
    let mut i = start;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    i - start
}

/// Detect an ATX heading marker (`#`–`######` + one separating space/tab) at `line_start` (leading
/// indentation tolerated). Returns `(level, body_start, line_end)`, or `None` if not a heading.
pub fn heading_at(source: &str, line_start: u32) -> Option<(u8, u32, u32)> {
    let bytes = source.as_bytes();
    let mut i = line_start as usize;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    let run_start = i;
    while i < bytes.len() && bytes[i] == b'#' {
        i += 1;
    }
    let level = i - run_start;
    if !(1..=6).contains(&level) || i >= bytes.len() || !matches!(bytes[i], b' ' | b'\t') {
        return None;
    }
    Some((level as u8, i as u32 + 1, line_content_end(source, line_start)))
}

/// Classify a list marker at the first non-whitespace of the line at `line_start` (`- `/`+ `/`N. `).
/// Returns the [`ListMarker`] (kind, indent depth, marker offset, body column), or `None`.
pub fn list_marker_at(source: &str, line_start: u32) -> Option<ListMarker> {
    let bytes = source.as_bytes();
    let mut i = line_start as usize;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    let indent = (i - line_start as usize) as u32;
    let offset = i as u32;
    if i >= bytes.len() {
        return None;
    }
    match bytes[i] {
        b'-' | b'+' if i + 1 < bytes.len() && bytes[i + 1] == b' ' => {
            let ordered = bytes[i] == b'+';
            Some(ListMarker { ordered, indent, offset, body_col: i as u32 + 2 })
        }
        b'0'..=b'9' => {
            let mut j = i;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'.' && j + 1 < bytes.len() && bytes[j + 1] == b' ' {
                Some(ListMarker { ordered: true, indent, offset, body_col: j as u32 + 2 })
            } else {
                None
            }
        }
        _ => None,
    }
}

/// The end offset of a list item's body: subsequent lines indented strictly past `marker_indent` (or
/// blank) belong to the item; it ends at the first line at/below `marker_indent` that is non-blank.
pub fn list_item_extent(source: &str, first_line_end: u32, marker_indent: u32) -> u32 {
    let bytes = source.as_bytes();
    let mut end = next_line_start(source, first_line_end);
    loop {
        if end as usize >= bytes.len() {
            break;
        }
        let line_start = end as usize;
        let mut i = line_start;
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        let is_blank = i >= bytes.len() || bytes[i] == b'\n';
        let indent = (i - line_start) as u32;
        if !is_blank && list_marker_at(source, end).is_some() && indent <= marker_indent {
            break;
        }
        if is_blank || indent > marker_indent {
            end = next_line_start(source, end);
        } else {
            break;
        }
    }
    end
}

/// Does the line at `line_start` open a `%`/`%%%` statement (first non-whitespace is `%`)?
pub fn is_statement_line(source: &str, line_start: u32) -> bool {
    let bytes = source.as_bytes();
    let mut i = line_start as usize;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    i < bytes.len() && bytes[i] == b'%'
}

/// Is the `%` statement whose body begins at `content` a no-op (rest-of-line empty/whitespace, or a
/// `//` line comment)? Such a line yields no statement; the collector skips it.
pub fn percent_line_is_empty(source: &str, content: u32) -> bool {
    let bytes = source.as_bytes();
    let line_end = line_content_end(source, content) as usize;
    let mut i = content as usize;
    while i < line_end && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    i >= line_end || (bytes.get(i) == Some(&b'/') && bytes.get(i + 1) == Some(&b'/'))
}

/// The start of the next line after `content`'s whose first non-whitespace is `%` (a statement
/// delimiter bounding the current `%` statement's JS parse), or the source length if none.
pub fn next_percent_line_or_end(source: &str, content: u32) -> u32 {
    let len = source.len() as u32;
    let mut line = next_line_start(source, content);
    while line < len {
        if is_statement_line(source, line) {
            return line;
        }
        line = next_line_start(source, line);
    }
    len
}

/// Classify the statement line at `line_start`. Returns `(content_or_inner_start, is_fence)`: for a
/// `%%%` fence, the offset of the line *after* the opener; for a `%` statement, just past the `%`.
pub fn statement_kind(source: &str, line_start: u32) -> Option<(u32, bool)> {
    let bytes = source.as_bytes();
    let mut i = line_start as usize;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    if i >= bytes.len() || bytes[i] != b'%' {
        return None;
    }
    let run_start = i;
    while i < bytes.len() && bytes[i] == b'%' {
        i += 1;
    }
    let run_len = i - run_start;
    if run_len >= 3 {
        let mut j = i;
        while j < bytes.len() && bytes[j] != b'\n' {
            if bytes[j] != b' ' && bytes[j] != b'\t' && bytes[j] != b'\r' {
                return Some((run_start as u32 + 1, false));
            }
            j += 1;
        }
        let inner_start = if j < bytes.len() { j as u32 + 1 } else { j as u32 };
        return Some((inner_start, true));
    }
    Some((run_start as u32 + 1, false))
}

/// Find the `%%%` fence close at/after `inner_start`. Returns `(inner_end, after_fence)`: the
/// closing-fence line start, and the offset past that line (the resume point).
pub fn find_fence_close(source: &str, inner_start: u32) -> (u32, u32) {
    let bytes = source.as_bytes();
    let mut line_start = inner_start as usize;
    while line_start < bytes.len() {
        let mut i = line_start;
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        let run_start = i;
        while i < bytes.len() && bytes[i] == b'%' {
            i += 1;
        }
        if i - run_start >= 3 {
            let mut j = i;
            while j < bytes.len() && bytes[j] != b'\n' {
                j += 1;
            }
            let after = if j < bytes.len() { j + 1 } else { j };
            return (line_start as u32, after as u32);
        }
        let mut j = line_start;
        while j < bytes.len() && bytes[j] != b'\n' {
            j += 1;
        }
        line_start = if j < bytes.len() { j + 1 } else { j };
    }
    (bytes.len() as u32, bytes.len() as u32)
}

/// Compute the source extent `[start, end)` of a `@head:` colon-sugar body.
///
/// The rest of the `@head:` line (from `colon_end`, after its one separating space) plus subsequent
/// lines indented strictly past `head_indent`. When `clip_at_brace`, a depth-0 `}` (closing an
/// enclosing `{…}` body) ends the first line early; `\{`/`\}` escapes are skipped.
pub fn colon_block_extent(
    source: &str,
    colon_end: u32,
    head_indent: usize,
    clip_at_brace: bool,
) -> (u32, u32) {
    let bytes = source.as_bytes();
    let mut start = colon_end as usize;
    while start < bytes.len() && (bytes[start] == b' ' || bytes[start] == b'\t') {
        start += 1;
    }
    let start = start as u32;
    let mut depth = 0i32;
    let mut j = colon_end as usize;
    let first_line_end = loop {
        match bytes.get(j) {
            None | Some(b'\n') => break next_line_start(source, colon_end),
            Some(b'\\') => j += 1, // skip the escaped byte
            Some(b'{') => depth += 1,
            Some(b'}') if depth == 0 && clip_at_brace => return (start, j as u32),
            Some(b'}') if depth > 0 => depth -= 1,
            _ => {}
        }
        j += 1;
    };
    let mut end = first_line_end;
    loop {
        if end as usize >= bytes.len() {
            break;
        }
        let line_start = end as usize;
        let mut i = line_start;
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        let is_blank = i >= bytes.len() || bytes[i] == b'\n';
        let indent = i - line_start;
        if is_blank || indent > head_indent {
            end = next_line_start(source, end);
        } else {
            break;
        }
    }
    (start, end)
}

// ================================================================================================
// Emphasis (`*`/`_`) — the Typst word-boundary rules and the close-matching scan. `next_nota_child`
// uses `emphasis_can_open` to classify an *opener*; the parser's `parse_emphasis` uses
// `find_emphasis_close` to find the matching close (skipping nested raw spans / `@`-forms / braces),
// then builds the AST.
// ================================================================================================

/// Is `c` "wordy" for the emphasis word-boundary rule (Typst `in_word`): alphanumeric, CJK excluded.
/// `None` (start/end of source) is not wordy, so a marker at a boundary opens/closes. (CJK exclusion
/// is approximated by Unicode block ranges — no `unicode-script` dep; ASCII + Latin/Greek/Cyrillic
/// classify exactly.)
fn is_wordy(c: Option<char>) -> bool {
    match c {
        None => false,
        Some(c) => c.is_alphanumeric() && !is_cjk(c),
    }
}

/// Approximate the CJK scripts Typst excludes from `in_word` (Han/Hiragana/Katakana/Hangul).
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF        // Hiragana + Katakana
        | 0x3400..=0x4DBF      // CJK Ext A
        | 0x4E00..=0x9FFF      // CJK Unified
        | 0xAC00..=0xD7AF      // Hangul syllables
        | 0xF900..=0xFAFF      // CJK compat
        | 0x20000..=0x2FA1F    // CJK Ext B+ / compat supplement
    )
}

/// The `char` ending at byte `offset` (immediately *before* it), or `None` at source start.
fn char_before(source: &str, offset: u32) -> Option<char> {
    if offset == 0 {
        return None;
    }
    source.get(..offset as usize).and_then(|s| s.chars().next_back())
}

/// The `char` starting at byte `offset`, or `None` at/after end of source.
fn char_at(source: &str, offset: u32) -> Option<char> {
    source.get(offset as usize..).and_then(|s| s.chars().next())
}

/// Is the byte at `off` preceded by an *odd* run of backslashes (i.e. escaped)?
fn is_escaped(source: &str, off: u32) -> bool {
    let bytes = source.as_bytes();
    let mut n = 0usize;
    let mut i = off as usize;
    while i > 0 && bytes[i - 1] == b'\\' {
        n += 1;
        i -= 1;
    }
    n % 2 == 1
}

/// Can a `*`/`_` at byte `off` in `source` **open** an emphasis span?
///
/// Used by `next_nota_child`'s marker classification (Typst's `'*' if !in_word()`). The marker must be
/// unescaped, not intra-word (the word-boundary rule), and immediately followed by *content* — a
/// non-whitespace byte that is not another copy of the same marker — so `* x`, runs `**`/`***`, and
/// intra-word `a*b` stay literal. (Whether a matching *close* exists is decided later by
/// [`find_emphasis_close`].)
pub fn emphasis_can_open(source: &str, off: usize, marker: u8) -> bool {
    let bytes = source.as_bytes();
    if is_escaped(source, off as u32) {
        return false;
    }
    let prev = char_before(source, off as u32);
    let next = char_at(source, off as u32 + 1);
    if is_wordy(prev) && is_wordy(next) {
        return false;
    }
    matches!(bytes.get(off + 1), Some(&b) if !b.is_ascii_whitespace() && b != marker)
}

/// Is the `*`/`_` at `marker_off` a significant marker (unescaped and not intra-word, the Typst rule)?
fn is_marker(source: &str, marker_off: u32) -> bool {
    if is_escaped(source, marker_off) {
        return false;
    }
    !(is_wordy(char_before(source, marker_off)) && is_wordy(char_at(source, marker_off + 1)))
}

/// Can a `*`/`_` at `off` **close** an emphasis span? A marker immediately *preceded* by content
/// (a non-whitespace byte), so `foo *` does not close (Typst). Non-emptiness is enforced by the caller.
fn can_close(source: &str, off: u32) -> bool {
    if !is_marker(source, off) {
        return false;
    }
    match (off as usize).checked_sub(1).and_then(|p| source.as_bytes().get(p)) {
        Some(&b) => !b.is_ascii_whitespace(),
        None => false,
    }
}

/// Skip a balanced bracket group (`(…)`/`[…]`/`{…}`, nesting all three) whose opener is at `at`,
/// returning the offset just past the matching closer, or `at + 1` if unterminated. Brackets only —
/// string/comment contents are not interpreted.
fn skip_balanced(source: &str, at: usize) -> usize {
    let bytes = source.as_bytes();
    let mut depth = 0u32;
    let mut i = at;
    while i < bytes.len() {
        match bytes[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    at + 1
}

/// Skip an `@`-form whose `@` is at `at`, returning the offset past the head and any adjacent
/// `(…)`/`[…]` group — so a `*`/`_` inside an embedded expression cannot close an emphasis, and a
/// stray bracket inside that JS cannot perturb the caller's brace depth. A trailing `{…}` markup body
/// is left to the caller's depth-tracked scan.
fn skip_at_form(source: &str, at: usize) -> usize {
    let bytes = source.as_bytes();
    let mut i = at + 1; // past '@'
    while i < bytes.len()
        && (bytes[i].is_ascii_alphanumeric()
            || matches!(bytes[i], b'_' | b'$' | b'.')
            || bytes[i] >= 0x80)
    {
        i += 1;
    }
    while matches!(bytes.get(i), Some(b'(' | b'[')) {
        i = skip_balanced(source, i);
    }
    i
}

/// Skip a raw span (inline/fenced code, math, or `|{ … }|` verbatim) whose opener byte is at `at`,
/// returning the offset just past its close (or `at + 1` if it has no valid close — the opener was
/// literal). Used by [`find_emphasis_close`] so emphasis matching steps over raw content.
fn skip_raw_span(source: &str, at: usize) -> usize {
    let bytes = source.as_bytes();
    match bytes[at] {
        b'`' => {
            let mut k = at;
            while k < bytes.len() && bytes[k] == b'`' {
                k += 1;
            }
            let fence_len = k - at;
            match find_backtick_close(source, k, fence_len) {
                Some(close) => close + fence_len,
                None => at + 1,
            }
        }
        b'$' => {
            let display = bytes.get(at + 1) == Some(&b'$');
            let delim = if display { 2 } else { 1 };
            let mut k = at + delim;
            while k < bytes.len() {
                match bytes[k] {
                    b'\\' => k += 2,
                    b'$' if !display => return k + 1,
                    b'$' if display && bytes.get(k + 1) == Some(&b'$') => return k + 2,
                    _ => k += 1,
                }
            }
            at + 1
        }
        b'|' => {
            let mut k = at + 2;
            while k < bytes.len() {
                if bytes[k] == b'}' && bytes.get(k + 1) == Some(&b'|') {
                    return k + 2;
                }
                k += 1;
            }
            at + 1
        }
        _ => at + 1,
    }
}

/// Find the matching close marker for an emphasis opened at `open` (raw offset of the marker), or `None`.
///
/// Scans forward for the next valid close `marker`, bounded by the emphasis *scope*: stops at a blank
/// line (paragraph break), at the `}` closing the enclosing body (depth below open level), or EOF.
/// Nested balanced `{…}`, raw spans, and `@`-forms are skipped so their inner `*`/`_` can't close.
pub fn find_emphasis_close(source: &str, open: u32, marker: u8) -> Option<u32> {
    let bytes = source.as_bytes();
    let mut i = open as usize + 1;
    let mut depth: i32 = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'\\' => i += 2, // escape: skip the escaped char (so `\*` cannot close)
            b'\n' => {
                // A blank line (this `\n`, optional ws, another `\n`) ends the scope.
                let mut j = i + 1;
                while j < bytes.len() && matches!(bytes[j], b' ' | b'\t' | b'\r') {
                    j += 1;
                }
                if j >= bytes.len() || bytes[j] == b'\n' {
                    return None;
                }
                i += 1;
            }
            b'{' => {
                depth += 1;
                i += 1;
            }
            b'}' => {
                if depth == 0 {
                    return None; // enclosing body closes before a matching marker
                }
                depth -= 1;
                i += 1;
            }
            b'`' | b'$' => i = skip_raw_span(source, i),
            b'|' if bytes.get(i + 1) == Some(&b'{') => i = skip_raw_span(source, i),
            b'@' => i = skip_at_form(source, i),
            _ if b == marker && depth == 0 => {
                if i as u32 > open + 1 && can_close(source, i as u32) {
                    return Some(i as u32);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    None
}

/// The result of scanning a `` ` ``-opened code span ([`lex_code_span`]).
pub enum CodeScan<'a> {
    /// A code span: `span` covers the whole `` `…` `` (block: opener…close run); `lang` is the block
    /// info-string's first token (inline → `None`); `content` is the raw inner text. Resume at `resume`.
    Code { span: Span, is_block: bool, lang: Option<&'a str>, content: &'a str, resume: u32 },
    /// Not a valid opener — the backtick `run` is literal text; resume just past it.
    Literal { run: &'a str, resume: u32 },
}

/// Scan a code span whose opening backtick run starts at `tick_off`.
///
/// A `≥3` run that is the last non-whitespace on its line (modulo a trailing language tag) is a
/// *fenced block*; otherwise it is inline code closed by the next run of `≥ fence_len` backticks. With
/// no close the run is literal.
pub fn lex_code_span(source: &str, tick_off: u32) -> CodeScan<'_> {
    let bytes = source.as_bytes();
    let mut i = tick_off as usize;
    while i < bytes.len() && bytes[i] == b'`' {
        i += 1;
    }
    let fence_len = i - tick_off as usize;
    let content_start = i;

    if fence_len >= 3
        && let Some(code) = scan_fenced_code(source, tick_off, fence_len, content_start)
    {
        return code;
    }

    // Inline code: content up to the next run of ≥ fence_len backticks (shorter runs are literal).
    if let Some(close) = find_backtick_close(source, content_start, fence_len) {
        let resume = close as u32 + fence_len as u32;
        return CodeScan::Code {
            span: Span::new(tick_off, resume),
            is_block: false,
            lang: None,
            content: &source[content_start..close],
            resume,
        };
    }
    CodeScan::Literal {
        run: &source[tick_off as usize..content_start],
        resume: content_start as u32,
    }
}

/// Find the next run of *at least* `fence_len` backticks at/after `from` (the offset of its first
/// backtick), or `None`. Shorter runs are literal content and are skipped.
pub fn find_backtick_close(source: &str, from: usize, fence_len: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let run_start = i;
            while i < bytes.len() && bytes[i] == b'`' {
                i += 1;
            }
            if i - run_start >= fence_len {
                return Some(run_start);
            }
        } else {
            i += 1;
        }
    }
    None
}

/// Scan a fenced code block opened by a `fence_len`-backtick run at `tick_off`, `content_start` just
/// past it. The opener-line tail (no backticks) is the optional language tag; the block ends at a line
/// whose first non-whitespace is a run of `≥ fence_len` backticks (or EOF). `None` if the opener line
/// is not a bare fence (then the caller treats it as inline code).
fn scan_fenced_code(
    source: &str,
    tick_off: u32,
    fence_len: usize,
    content_start: usize,
) -> Option<CodeScan<'_>> {
    let bytes = source.as_bytes();
    let mut j = content_start;
    while j < bytes.len() && bytes[j] != b'\n' {
        if bytes[j] == b'`' {
            return None; // backticks on the opener line ⇒ not a fenced block (inline run)
        }
        j += 1;
    }
    // Language = FIRST token of the info string (`` ```js extra `` → `js`).
    let lang_str = source[content_start..j].split_whitespace().next().unwrap_or("");
    let lang = (!lang_str.is_empty()).then_some(lang_str);
    if j >= bytes.len() {
        return None; // no newline after the opener ⇒ not a block
    }
    let body_start = j + 1;

    let mut line_start = body_start;
    loop {
        if line_start >= bytes.len() {
            // Unterminated fence: code runs to EOF.
            return Some(CodeScan::Code {
                span: Span::new(tick_off, bytes.len() as u32),
                is_block: true,
                lang,
                content: &source[body_start..bytes.len()],
                resume: bytes.len() as u32,
            });
        }
        let mut k = line_start;
        while k < bytes.len() && (bytes[k] == b' ' || bytes[k] == b'\t') {
            k += 1;
        }
        let run_start = k;
        while k < bytes.len() && bytes[k] == b'`' {
            k += 1;
        }
        if k - run_start >= fence_len {
            // Close fence: body is [body_start, line_start) minus the preceding `\n`. Resume right
            // after the backtick run (trailing content — e.g. a `}` closing an enclosing body — is
            // left for the collector).
            let mut code_end = line_start;
            if code_end > body_start && bytes[code_end - 1] == b'\n' {
                code_end -= 1;
            }
            return Some(CodeScan::Code {
                span: Span::new(tick_off, k as u32),
                is_block: true,
                lang,
                content: &source[body_start..code_end],
                resume: k as u32,
            });
        }
        line_start = next_line_start(source, line_start as u32) as usize;
    }
}
