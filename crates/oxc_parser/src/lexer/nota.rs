//! Lexer scan-methods for Nota `@`-markup.
//!
//! Nota is markup-outer, JS-embedded (the inverse of JSX). [`Lexer::next_nota_child`] is the markup
//! analog of `next_jsx_child` (and of Typst's markup-mode lexer): the parser drives the lexer into
//! markup-body mode and pulls one child token at a time — a maximal literal-text run
//! ([`Kind::MarkupText`]) or a single markup *sigil* as a typed, consumed token — and dispatches on
//! its [`Kind`], never on raw bytes. The token's text is read via `token_source` (the raw source
//! slice), which keeps embedded spans byte-identical with the source.
//!
//! The free functions below are the reader's *scans*: pure `(source, offset) → offsets/spans/&str`
//! lookups the parser calls to find extents (lines, statements, blocks, emphasis closes, raw spans).
//! They never touch the lexer cursor; all raw byte-munging lives here so the parser can stay on
//! typed tokens and AST construction.

// Source offsets and substring lengths are cast to `u32` throughout: oxc's `Span` is `u32`-based
// (sources are bounded to 4 GiB), so these `as u32` casts cannot truncate in practice.
#![expect(
    clippy::cast_possible_truncation,
    reason = "source offsets/lengths fit in u32 (oxc's Span model)"
)]

use lazy_regex::{Lazy, Regex, lazy_regex};
use oxc_span::Span;
use oxc_syntax::identifier::{is_identifier_part, is_identifier_start};
use unicode_script::{Script, UnicodeScript};

use super::{
    Kind, Lexer, Token,
    search::{SafeByteMatchTable, byte_search, safe_byte_match_table},
};
use crate::config::LexerConfig as Config;

/// Bytes that terminate a Nota markup-text run — the markup sigils [`Lexer::next_nota_child`]
/// returns as their own typed tokens.
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
    /// Pull one Nota markup-body *child token* at the current source position.
    ///
    /// Returns a maximal literal-text run ([`Kind::MarkupText`], ≥1 byte) or, when positioned on a
    /// markup sigil, that sigil *consumed* as a typed token: `@`→[`Kind::At`], `{`→[`Kind::LCurly`],
    /// `}`→[`Kind::RCurly`], `\n`→[`Kind::NotaNewline`], `*`→[`Kind::Star`],
    /// `_`→[`Kind::NotaUnderscore`], `\`→[`Kind::NotaBackslash`], `` ` ``→[`Kind::NotaBacktick`],
    /// `$`→[`Kind::NotaDollar`], `|`→[`Kind::Pipe`]. For sigils whose semantics span more than the
    /// one byte (code/math/emphasis/escape), the parser's scan helper re-scans from the token start
    /// and re-seeks.
    pub(crate) fn next_nota_child(&mut self) -> Token {
        let start = self.offset();
        self.token.set_start(start);

        let kind = match self.peek_byte() {
            Some(b'@') => Kind::At,
            Some(b'{') => Kind::LCurly,
            Some(b'}') => Kind::RCurly,
            Some(b'\n') => Kind::NotaNewline,
            // Emphasis: a marker token only at a valid opener (the Typst word-boundary rule);
            // otherwise the `*`/`_` is a 1-byte text token. The matching *close* is resolved by the
            // parser via `find_emphasis_close`.
            Some(b @ (b'*' | b'_')) => {
                let kind = if emphasis_can_open(self.source.whole(), start, b) {
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
            // Literal text: maximal run up to (not including) the next sigil.
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
        // Every sigil byte above is ASCII, so this consumes exactly one byte.
        self.consume_char();
        self.finish_re_lex(kind)
    }

    /// Lex a Nota `@`-form *head* identifier (`@foo`, `@if`, `@café`).
    ///
    /// Unlike JS identifier lexing, a `\` is not a `\u` escape here — it simply terminates the head
    /// (so `@foo\:` is the head `foo` followed by the literal escape `\:`). Keyword heads route
    /// through [`Kind::match_keyword`], so `@if`/`@for` still produce [`Kind::If`]/[`Kind::For`].
    /// Entered positioned at the head's first char (just past `@`), which must be an
    /// identifier-start; leaves the source at the first non-identifier char.
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
// Shared classifications (produced by the scans below, consumed by the parser)
// ================================================================================================

/// The element trigger immediately following an `@`-form head (see [`markup_trigger`]).
#[derive(Clone, Copy)]
pub enum MarkupTrigger {
    /// `@head{…}` — a `{`-body element.
    Brace,
    /// `@head[…]` — a `[props]` element (body optional).
    Bracket,
    /// `@head:…` — colon / block sugar.
    Colon,
    /// `@head|{…}|` — a verbatim body.
    Verbatim,
    /// No trigger glued to the head ⇒ interpolation (`@name` / `@(expr)`).
    None,
}

/// A list marker found at a line start (see [`list_marker_at`]).
pub struct ListMarker {
    /// `true` for an ordered marker (`+` / `N.`); `false` for a bullet (`-`).
    pub ordered: bool,
    /// Indentation *depth* (leading-whitespace count), not a byte offset — depth is what drives
    /// nesting when sibling markers sit at different byte offsets.
    pub indent: u32,
    /// Byte offset of the marker's first char — the item's source start.
    pub offset: u32,
    /// Offset where the item body begins (just past the marker and its one separating space).
    pub body_col: u32,
}

/// The result of scanning for an `else`/`else if` continuation after an `@if` branch
/// (see [`else_peek`]).
pub enum ElsePeek {
    /// No continuation (the alternate is `null`).
    None,
    /// `else if (…) {…}` — resume parsing at the `if` keyword.
    ElseIf { if_offset: u32 },
    /// `else {…}` — resume parsing at the body `{`.
    Else { brace_offset: u32 },
}

// ================================================================================================
// Byte primitives
// ================================================================================================

/// Peek the raw byte at `offset`, or `None` at/after end of source.
pub fn byte_at(source: &str, offset: u32) -> Option<u8> {
    source.as_bytes().get(offset as usize).copied()
}

/// Is the char at `off` an identifier-start? Decides whether an `@`-head is lexed with Nota
/// identifier rules ([`Lexer::next_nota_head`]) or stays on the JS-lexed path (`@(expr)`, `@{…}`).
pub fn is_ident_start_at(source: &str, off: u32) -> bool {
    source[off as usize..].chars().next().is_some_and(is_identifier_start)
}

/// Skip spaces/tabs from `i`; returns the first non-inline-whitespace offset.
fn skip_inline_ws(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t') {
        i += 1;
    }
    i
}

/// The end of the run of byte `b` starting at `i`.
fn skip_run(bytes: &[u8], mut i: usize, b: u8) -> usize {
    while i < bytes.len() && bytes[i] == b {
        i += 1;
    }
    i
}

/// The end of the run of `byte` starting at `off` (for literal sigil-run fallbacks).
pub fn sigil_run_end(source: &str, off: u32, byte: u8) -> u32 {
    skip_run(source.as_bytes(), off as usize, byte) as u32
}

/// `(content_offset, is_blank)` for the line starting at `line_start`: the offset of its first
/// non-inline-whitespace byte, and whether the line has no content.
fn line_probe(source: &str, line_start: u32) -> (usize, bool) {
    let bytes = source.as_bytes();
    let i = skip_inline_ws(bytes, line_start as usize);
    (i, i >= bytes.len() || bytes[i] == b'\n')
}

// ================================================================================================
// Line geometry
// ================================================================================================

/// The offset of the terminating `\n` of the line containing `from` (or EOF if none).
pub fn line_content_end(source: &str, from: u32) -> u32 {
    match memchr::memchr(b'\n', &source.as_bytes()[from as usize..]) {
        Some(k) => from + k as u32,
        None => source.len() as u32,
    }
}

/// The line containing `from`, from `from` up to (not including) its terminating `\n` — the slice
/// the line-classifier regexes below match against.
fn line_at(source: &str, from: u32) -> &str {
    &source[from as usize..line_content_end(source, from) as usize]
}

/// Offset just past the next `\n` at/after `offset` (or EOF if none) — the start of the next line.
pub fn next_line_start(source: &str, offset: u32) -> u32 {
    let end = line_content_end(source, offset);
    if (end as usize) < source.len() { end + 1 } else { end }
}

/// The indentation (leading space/tab count) of the line containing byte `offset`.
pub fn line_indent_of(source: &str, offset: u32) -> usize {
    let bytes = source.as_bytes();
    let mut start = offset as usize;
    while start > 0 && bytes[start - 1] != b'\n' {
        start -= 1;
    }
    skip_inline_ws(bytes, start) - start
}

/// The end of an indentation-scoped block: starting at line `line_start`, consume lines that are
/// blank or indented strictly past `min_indent`; return the first line at/under it (or EOF).
/// This is the shared block-extent rule for list-item bodies and colon-sugar continuations.
fn indented_block_end(source: &str, mut line_start: u32, min_indent: u32) -> u32 {
    while (line_start as usize) < source.len() {
        let (content, is_blank) = line_probe(source, line_start);
        let indent = content as u32 - line_start;
        if !is_blank && indent <= min_indent {
            break;
        }
        line_start = next_line_start(source, line_start);
    }
    line_start
}

// ================================================================================================
// Head scans (`@`-form head → trigger classification)
// ================================================================================================

/// Scan a hyphenated custom-element name tail (`@my-widget`, `@x-y-z`) at `at`: one or more
/// `-`-joined runs of identifier chars. Returns the offset past the tail, or `None` if `at` is not
/// a `-` directly followed by an identifier char. (The JS lexer stops a bare identifier at `-`, so
/// the tail is read here — the markup analog of JSX's `continue_lex_jsx_identifier`.)
pub fn scan_hyphen_tail(source: &str, at: u32) -> Option<u32> {
    let bytes = source.as_bytes();
    let mut i = at as usize;
    let mut consumed = false;
    while bytes.get(i) == Some(&b'-')
        && bytes.get(i + 1).is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
    {
        i += 1;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        consumed = true;
    }
    consumed.then_some(i as u32)
}

/// Classify the byte at `after` as the trigger glued to an `@`-form head. This is the one
/// whitespace-sensitive byte peek at the head→body boundary: the byte after a head is not a JS
/// token (a space is significant, and `|{` is not lexable), so it is peeked rather than lexed.
pub fn markup_trigger(source: &str, after: u32) -> MarkupTrigger {
    let bytes = source.as_bytes();
    match bytes.get(after as usize) {
        Some(b'{') => MarkupTrigger::Brace,
        Some(b'[') => MarkupTrigger::Bracket,
        Some(b':') => MarkupTrigger::Colon,
        Some(b'|') if bytes.get(after as usize + 1) == Some(&b'{') => MarkupTrigger::Verbatim,
        _ => MarkupTrigger::None,
    }
}

// ================================================================================================
// Escapes & keywords
// ================================================================================================

/// The span of the literal a backslash escape at `esc_off` produces: `\<c>` → the `<c>` slice (the
/// `\` dropped); a trailing lone `\` at EOF → the `\` itself. Markup resumes at `span.end`.
pub fn escape_span(source: &str, esc_off: u32) -> Span {
    let start = esc_off + 1;
    match source[start as usize..].chars().next() {
        Some(c) => Span::new(start, start + c.len_utf8() as u32),
        None => Span::new(esc_off, start),
    }
}

/// Does `source[at..]` begin with keyword `kw` followed by a word boundary (so `else`/`if` match
/// but `elsewhere`/`iffy` do not)?
fn matches_keyword(source: &str, at: usize, kw: &[u8]) -> bool {
    let bytes = source.as_bytes();
    if at + kw.len() > bytes.len() || &bytes[at..at + kw.len()] != kw {
        return false;
    }
    match bytes.get(at + kw.len()) {
        Some(b) => !(b.is_ascii_alphanumeric() || matches!(b, b'_' | b'$') || *b >= 0x80),
        None => true,
    }
}

/// Scan for an `else`/`else if` continuation after an `@if` branch that closed at `close_end`:
/// skip inline whitespace and at most one newline (a blank line breaks the chain), reject an
/// escaped `\else`, then classify what follows `else` (`if` → else-if, `{` → else-block).
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
                    return ElsePeek::None;
                }
                i += 1;
            }
            _ => break,
        }
    }
    if bytes.get(i) == Some(&b'\\') || !matches_keyword(source, i, b"else") {
        return ElsePeek::None;
    }
    let mut j = i + 4;
    while j < bytes.len() && matches!(bytes[j], b' ' | b'\t' | b'\r' | b'\n') {
        j += 1;
    }
    if matches_keyword(source, j, b"if") {
        ElsePeek::ElseIf { if_offset: j as u32 }
    } else if bytes.get(j) == Some(&b'{') {
        ElsePeek::Else { brace_offset: j as u32 }
    } else {
        ElsePeek::None
    }
}

// ================================================================================================
// Line-start constructs: `%`/`%%%` statements, headings, lists, colon-sugar prop lines
// ================================================================================================

/// A line whose first non-whitespace is `%` (a `%`/`%%%` statement line).
static PERCENT_LINE: Lazy<Regex> = lazy_regex!(r"^[ \t]*%");
/// A `%%%` fence line: a run of ≥3 `%` alone on its line.
static FENCE_LINE: Lazy<Regex> = lazy_regex!(r"^[ \t]*%{3,}[ \t\r]*$");
/// A `%%%`-or-longer run at line start (a fence *close* tolerates trailing content).
static FENCE_CLOSE_LINE: Lazy<Regex> = lazy_regex!(r"^[ \t]*%{3,}");
/// A `%` statement body that is a no-op: empty/whitespace, or only a `//` line comment.
static EMPTY_STATEMENT: Lazy<Regex> = lazy_regex!(r"^[ \t]*(//.*)?$");

/// Does the line at `line_start` open a `%`/`%%%` statement (first non-whitespace is `%`)?
pub fn is_statement_line(source: &str, line_start: u32) -> bool {
    PERCENT_LINE.is_match(line_at(source, line_start))
}

/// Classify the statement line at `line_start`. Returns `(content_or_inner_start, is_fence)`: for
/// a `%%%` fence line (a `≥3` run alone on its line), the offset of the line *after* the opener;
/// for a `%` statement, the offset just past the first `%`.
pub fn statement_kind(source: &str, line_start: u32) -> Option<(u32, bool)> {
    let line = line_at(source, line_start);
    if FENCE_LINE.is_match(line) {
        return Some((next_line_start(source, line_start), true));
    }
    let m = PERCENT_LINE.find(line)?;
    Some((line_start + m.end() as u32, false))
}

/// Is the `%` statement whose body begins at `content` a no-op (rest-of-line empty/whitespace, or
/// only a `//` line comment)? Such a line yields no statement.
pub fn percent_line_is_empty(source: &str, content: u32) -> bool {
    EMPTY_STATEMENT.is_match(line_at(source, content))
}

/// The start of the next line after `content`'s whose first non-whitespace is `%` (the delimiter
/// bounding the current `%` statement's JS parse), or the source length if none.
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

/// Find the `%%%` fence close at/after `inner_start`. Returns `(inner_end, after_fence)`: the
/// closing-fence line start, and the offset past that line (the resume point).
pub fn find_fence_close(source: &str, inner_start: u32) -> (u32, u32) {
    let mut line_start = inner_start;
    while (line_start as usize) < source.len() {
        if FENCE_CLOSE_LINE.is_match(line_at(source, line_start)) {
            return (line_start, next_line_start(source, line_start));
        }
        line_start = next_line_start(source, line_start);
    }
    (source.len() as u32, source.len() as u32)
}

/// An ATX heading marker: 1–6 `#` (captured) + one space/tab, leading indentation tolerated.
static HEADING: Lazy<Regex> = lazy_regex!(r"^[ \t]*(#{1,6})[ \t]");
/// A list marker line: indentation (captured), then `- ` / `+ ` / `N. ` (marker captured).
static LIST_MARKER: Lazy<Regex> = lazy_regex!(r"^([ \t]*)([-+]|[0-9]+\.) ");
/// A colon-sugar `|`-prop line: first non-whitespace is `|`.
static PROP_LINE: Lazy<Regex> = lazy_regex!(r"^[ \t]*\|");

/// Detect an ATX heading marker (1–6 `#` + one space/tab) at `line_start` (leading indentation
/// tolerated). Returns `(level, body_start, line_end)`, or `None` if not a heading.
pub fn heading_at(source: &str, line_start: u32) -> Option<(u8, u32, u32)> {
    let caps = HEADING.captures(line_at(source, line_start))?;
    let level = caps[1].len() as u8;
    Some((level, line_start + caps[0].len() as u32, line_content_end(source, line_start)))
}

/// Classify a list marker at the first non-whitespace of the line at `line_start`
/// (`- ` / `+ ` / `N. `), or `None`.
pub fn list_marker_at(source: &str, line_start: u32) -> Option<ListMarker> {
    let caps = LIST_MARKER.captures(line_at(source, line_start))?;
    let indent = caps[1].len() as u32;
    Some(ListMarker {
        ordered: &caps[2] != "-",
        indent,
        offset: line_start + indent,
        body_col: line_start + caps[0].len() as u32,
    })
}

/// The end offset of a list item's body: lines after the marker line that are blank or indented
/// strictly past `marker_indent` belong to the item.
pub fn list_item_extent(source: &str, first_line_end: u32, marker_indent: u32) -> u32 {
    indented_block_end(source, next_line_start(source, first_line_end), marker_indent)
}

/// If the line at `line_start` is a colon-sugar `|`-prop line (first non-whitespace is `|`),
/// return the offset just past the `|` (where the prop entries begin); else `None`.
pub fn colon_prop_line_at(source: &str, line_start: u32) -> Option<u32> {
    let m = PROP_LINE.find(line_at(source, line_start))?;
    Some(line_start + m.end() as u32)
}

/// Compute the source extent `[start, end)` of a `@head:` colon-sugar body: the rest of the
/// `@head:` line (after inline whitespace) plus following lines indented strictly past
/// `head_indent`. When `clip_at_brace`, a depth-0 `}` (closing an enclosing `{…}` body) ends the
/// body on the first line; `\`-escaped bytes are skipped, and an `@`-form's head + `(…)`/`[…]`
/// groups are opaque ([`skip_at_form`]) — a `}` inside embedded-JS props cannot clip the body.
pub fn colon_block_extent(
    source: &str,
    colon_end: u32,
    head_indent: usize,
    clip_at_brace: bool,
) -> (u32, u32) {
    let bytes = source.as_bytes();
    let start = skip_inline_ws(bytes, colon_end as usize) as u32;
    let mut depth = 0i32;
    let mut j = colon_end as usize;
    let first_line_end = loop {
        match bytes.get(j) {
            None | Some(b'\n') => break next_line_start(source, colon_end),
            Some(b'\\') => j += 2, // skip the escaped byte
            Some(b'@') => j = skip_at_form(source, j),
            Some(b'{') => {
                depth += 1;
                j += 1;
            }
            Some(b'}') if depth == 0 && clip_at_brace => return (start, j as u32),
            Some(b'}') => {
                depth = (depth - 1).max(0);
                j += 1;
            }
            _ => j += 1,
        }
    };
    (start, indented_block_end(source, first_line_end, head_indent as u32))
}

// ================================================================================================
// Emphasis (`*`/`_`) — the Typst word-boundary rule and the close-matching scan
// ================================================================================================

/// Is `c` "wordy" for the emphasis word-boundary rule (Typst `in_word`): alphanumeric, CJK
/// excluded (CJK has no word boundaries to respect). `None` (start/end of source) is not wordy.
fn is_wordy(c: Option<char>) -> bool {
    match c {
        None => false,
        Some(c) => c.is_alphanumeric() && !is_cjk(c),
    }
}

/// The CJK scripts Typst excludes from `in_word` (Han/Hiragana/Katakana/Hangul — scripts with no
/// word boundaries to respect), by Unicode `Script` property.
fn is_cjk(c: char) -> bool {
    matches!(c.script(), Script::Han | Script::Hiragana | Script::Katakana | Script::Hangul)
}

/// The `char` ending at byte `offset` (immediately before it), or `None` at source start.
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

/// Can a `*`/`_` at byte `off` **open** an emphasis span? It must be unescaped, not intra-word
/// (the Typst word-boundary rule), and immediately followed by content — a non-whitespace byte
/// that is not another copy of the marker — so `* x`, runs `**`, and intra-word `a*b` stay literal.
fn emphasis_can_open(source: &str, off: u32, marker: u8) -> bool {
    if is_escaped(source, off) {
        return false;
    }
    if is_wordy(char_before(source, off)) && is_wordy(char_at(source, off + 1)) {
        return false;
    }
    matches!(byte_at(source, off + 1), Some(b) if !b.is_ascii_whitespace() && b != marker)
}

/// Is the `*`/`_` at `marker_off` a significant marker (unescaped, not intra-word)?
fn is_marker(source: &str, marker_off: u32) -> bool {
    !(is_escaped(source, marker_off)
        || is_wordy(char_before(source, marker_off)) && is_wordy(char_at(source, marker_off + 1)))
}

/// Can a `*`/`_` at `off` **close** an emphasis span? A marker immediately preceded by content
/// (a non-whitespace byte), so `foo *` does not close.
fn can_close(source: &str, off: u32) -> bool {
    if !is_marker(source, off) {
        return false;
    }
    match (off as usize).checked_sub(1).and_then(|p| source.as_bytes().get(p)) {
        Some(&b) => !b.is_ascii_whitespace(),
        None => false,
    }
}

/// Skip a JS string or template literal whose opening quote is at `at`, honoring `\`-escapes.
/// Returns the offset just past the closing quote. An unterminated `'`/`"` stops at the newline
/// (the scan resumes there); a template runs to its closing backtick (newlines allowed, `${…}`
/// contents opaque). Unterminated at EOF → the source end.
fn skip_js_string(source: &str, at: usize) -> usize {
    let bytes = source.as_bytes();
    let quote = bytes[at];
    let mut i = at + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'\n' if quote != b'`' => return i,
            b if b == quote => return i + 1,
            _ => i += 1,
        }
    }
    bytes.len()
}

/// Skip a balanced bracket group (`(…)`/`[…]`/`{…}`, nesting all three) whose opener is at `at`,
/// returning the offset just past the matching closer, or `at + 1` if unterminated. The group is
/// embedded JS, so string/template and comment contents are skipped — a bracket or emphasis
/// marker inside `"…"`/`` `…` ``/`/*…*/` cannot unbalance it. (Regex literals are not recognized;
/// these scans bound heuristic extents — the embedded JS is properly parsed afterwards.)
fn skip_balanced(source: &str, at: usize) -> usize {
    let bytes = source.as_bytes();
    let mut depth = 0u32;
    let mut i = at;
    while i < bytes.len() {
        match bytes[i] {
            b'(' | b'[' | b'{' => {
                depth += 1;
                i += 1;
            }
            b')' | b']' | b'}' => {
                depth -= 1;
                i += 1;
                if depth == 0 {
                    return i;
                }
            }
            b'"' | b'\'' | b'`' => i = skip_js_string(source, i),
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                i = line_content_end(source, i as u32) as usize;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i = match memchr::memmem::find(&bytes[i + 2..], b"*/") {
                    Some(k) => i + 2 + k + 2,
                    None => bytes.len(),
                };
            }
            _ => i += 1,
        }
    }
    at + 1
}

/// Skip an `@`-form whose `@` is at `at`: past the head and any adjacent `(…)`/`[…]` groups — so a
/// `*`/`_` inside an embedded expression cannot close an emphasis, and a stray bracket inside that
/// JS cannot perturb the caller's brace depth. A trailing `{…}` markup body is left to the
/// caller's depth-tracked scan.
fn skip_at_form(source: &str, at: usize) -> usize {
    let bytes = source.as_bytes();
    let mut i = at + 1;
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
/// returning the offset just past its close, or `at + 1` if it has no valid close (the opener was
/// literal). Lets emphasis matching step over raw content.
fn skip_raw_span(source: &str, at: usize) -> usize {
    let bytes = source.as_bytes();
    match bytes[at] {
        b'`' => {
            let k = skip_run(bytes, at, b'`');
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

/// Find the matching close marker for an emphasis opened at `open`, or `None` (then the opener is
/// literal). Scans forward for the next valid close, bounded by the emphasis *scope*: a blank line
/// (paragraph break), the `}` closing the enclosing body, or EOF. Balanced `{…}`, raw spans, and
/// `@`-forms are skipped so their inner `*`/`_` cannot close.
pub fn find_emphasis_close(source: &str, open: u32, marker: u8) -> Option<u32> {
    let bytes = source.as_bytes();
    let mut i = open as usize + 1;
    let mut depth: i32 = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'\\' => i += 2, // skip the escaped char (so `\*` cannot close)
            b'\n' => {
                let j = skip_inline_ws_and_cr(bytes, i + 1);
                if j >= bytes.len() || bytes[j] == b'\n' {
                    return None; // blank line ends the scope
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

/// Skip spaces/tabs/`\r` from `i` (blank-line detection inside [`find_emphasis_close`]).
fn skip_inline_ws_and_cr(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\r') {
        i += 1;
    }
    i
}

// ================================================================================================
// Code spans (inline `` `…` `` and fenced ```` ```lang⏎…⏎``` ````)
// ================================================================================================

/// The result of scanning a `` ` ``-opened code span ([`lex_code_span`]).
pub enum CodeScan<'a> {
    /// A code span: `span` covers the whole `` `…` ``; `lang` is the block info-string's first
    /// token (inline → `None`); `content` is the raw inner text.
    Code { span: Span, is_block: bool, lang: Option<&'a str>, content: &'a str, resume: u32 },
    /// Not a valid opener — the backtick run is literal text ending at `resume`.
    Literal { resume: u32 },
}

/// Scan a code span whose opening backtick run starts at `tick_off`. A `≥3` run that is the last
/// non-whitespace on its line (modulo a language tag) opens a *fenced block*; otherwise inline
/// code closed by the next run of `≥ fence_len` backticks (shorter runs are literal). With no
/// close the run is literal.
pub fn lex_code_span(source: &str, tick_off: u32) -> CodeScan<'_> {
    let bytes = source.as_bytes();
    let content_start = skip_run(bytes, tick_off as usize, b'`');
    let fence_len = content_start - tick_off as usize;

    if fence_len >= 3
        && let Some(code) = scan_fenced_code(source, tick_off, fence_len, content_start)
    {
        return code;
    }

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
    CodeScan::Literal { resume: content_start as u32 }
}

/// Find the next run of at least `fence_len` backticks at/after `from` (the offset of its first
/// backtick), or `None`. Shorter runs are literal content.
fn find_backtick_close(source: &str, from: usize, fence_len: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let run_start = i;
            i = skip_run(bytes, i, b'`');
            if i - run_start >= fence_len {
                return Some(run_start);
            }
        } else {
            i += 1;
        }
    }
    None
}

/// Scan a fenced code block opened by a `fence_len`-backtick run at `tick_off` (`content_start`
/// just past it). The opener-line tail (which must contain no backticks) is the optional language
/// tag; the block ends at a line whose first non-whitespace is a run of `≥ fence_len` backticks
/// (or EOF). `None` if the opener line disqualifies (then it is inline code).
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
            return None; // backticks on the opener line ⇒ inline run, not a fence
        }
        j += 1;
    }
    if j >= bytes.len() {
        return None; // no newline after the opener ⇒ not a block
    }
    // Language = first token of the info string (```` ```js extra ```` → `js`).
    let lang = source[content_start..j].split_whitespace().next();
    let body_start = j + 1;

    let mut line_start = body_start;
    loop {
        if line_start >= bytes.len() {
            // Unterminated fence: code runs to EOF.
            return Some(CodeScan::Code {
                span: Span::new(tick_off, bytes.len() as u32),
                is_block: true,
                lang,
                content: &source[body_start..],
                resume: bytes.len() as u32,
            });
        }
        let run_start = skip_inline_ws(bytes, line_start);
        let k = skip_run(bytes, run_start, b'`');
        if k - run_start >= fence_len {
            // Close fence: body ends before the fence line's `\n`. Resume right after the backtick
            // run — trailing content (e.g. a `}` closing an enclosing body) is the collector's.
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

// ================================================================================================
// Math spans (`$…$` / `$$…$$`) — the raw-content boundary scan
// ================================================================================================

/// A boundary reached while scanning a math span's raw content ([`math_boundary`]).
pub enum MathBoundary {
    /// The closing `$`/`$$` run: the span ends; markup resumes at `after`.
    Close { after: u32 },
    /// `@name` — a lexical interpolation; the identifier spans `[run_end + 1, name_end)`.
    InterpName { name_end: u32 },
    /// `@(` — the parser parses the parenthesized expression (positioned at the `@`).
    InterpParen,
    /// `@` with no interpolation head — spliced as a literal `"@"`.
    LiteralAt,
    /// No closing delimiter before end of source (the opening `$` run is then literal).
    Eof,
}

/// Scan a math span's raw LaTeX from `from` to the next boundary: the closing delimiter, an
/// `@`-interpolation, or EOF. Returns `(run_end, boundary)` where `[from, run_end)` is raw
/// content: `\<c>` keeps its backslash (LaTeX's own escape, so `\$`/`\@` stay literal), and a
/// single `$` inside display math is literal. The `@name` scan is ASCII and excludes `$` so the
/// math delimiter wins (`@i$`).
pub fn math_boundary(source: &str, from: u32, display: bool) -> (u32, MathBoundary) {
    let bytes = source.as_bytes();
    let delim: u32 = if display { 2 } else { 1 };
    let mut i = from as usize;
    loop {
        match bytes.get(i) {
            None => return (bytes.len() as u32, MathBoundary::Eof),
            Some(b'\\') => i += 2,
            Some(b'$') if !display || bytes.get(i + 1) == Some(&b'$') => {
                return (i as u32, MathBoundary::Close { after: i as u32 + delim });
            }
            Some(b'@') => {
                let name_start = i + 1;
                let boundary = if bytes.get(name_start) == Some(&b'(') {
                    MathBoundary::InterpParen
                } else {
                    let name_end = skip_ascii_ident(bytes, name_start);
                    if name_end == name_start {
                        MathBoundary::LiteralAt
                    } else {
                        MathBoundary::InterpName { name_end: name_end as u32 }
                    }
                };
                return (i as u32, boundary);
            }
            Some(_) => i += 1,
        }
    }
}

/// The end of the ASCII identifier run (`[A-Za-z0-9_]*`) starting at `i`.
fn skip_ascii_ident(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
        i += 1;
    }
    i
}

// ================================================================================================
// Verbatim bodies (`|{ … }|`) — the raw-run boundary scan
// ================================================================================================

/// A boundary reached while scanning a verbatim body's raw run ([`verbatim_boundary`]).
pub enum VerbatimBoundary {
    /// The closing `}|`: the element resumes at `after`.
    Close { after: u32 },
    /// `|@` — an armed escape: the parser parses one `@`-form at `at` (the `@`).
    ArmedAt { at: u32 },
    /// Unterminated: no `}|` before end of source.
    Eof,
}

/// Scan a verbatim body's raw run from `from` to the next boundary: the closing `}|`, an armed
/// `|@` escape, or EOF. Returns `(run_end, boundary)` where `[from, run_end)` is the raw slice —
/// for a close, a single trailing newline right before `}|` is dropped (the `}`-newline rule).
pub fn verbatim_boundary(source: &str, from: u32) -> (u32, VerbatimBoundary) {
    let bytes = source.as_bytes();
    let mut i = from as usize;
    while i < bytes.len() {
        match bytes[i] {
            b'}' if bytes.get(i + 1) == Some(&b'|') => {
                let run_end = if i > from as usize && bytes[i - 1] == b'\n' { i - 1 } else { i };
                return (run_end as u32, VerbatimBoundary::Close { after: i as u32 + 2 });
            }
            b'|' if bytes.get(i + 1) == Some(&b'@') => {
                return (i as u32, VerbatimBoundary::ArmedAt { at: i as u32 + 1 });
            }
            _ => i += 1,
        }
    }
    (bytes.len() as u32, VerbatimBoundary::Eof)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The string-aware [`skip_balanced`]: a bracket or `*` inside a JS string/comment in an
    /// `@`-form's groups must not end the group early (nor let an emphasis close inside it).
    #[test]
    fn emphasis_close_skips_strings_in_at_form_groups() {
        // Old behavior: the `)` inside `")*"` ended the group scan mid-string, and the `*` right
        // after it (still inside the string) closed the emphasis.
        let src = r#"*a @f(")*") b*"#;
        assert_eq!(find_emphasis_close(src, 0, b'*'), Some(src.len() as u32 - 1));

        let src = r#"*a @f[x: "}*"] b*"#;
        assert_eq!(find_emphasis_close(src, 0, b'*'), Some(src.len() as u32 - 1));
    }

    #[test]
    fn emphasis_close_skips_comments_in_at_form_groups() {
        // The `)` and `*` inside the block comment are not group/emphasis structure.
        let src = "*a @f(/*)*/x) b*";
        assert_eq!(find_emphasis_close(src, 0, b'*'), Some(src.len() as u32 - 1));
    }

    #[test]
    fn emphasis_close_unterminated_string_stops_at_newline() {
        // An unterminated `"` must not swallow the rest of the body: the scan resumes at the
        // newline, and the blank line still ends the emphasis scope (→ literal opener).
        let src = "*a @f(\"x\n\n b*";
        assert_eq!(find_emphasis_close(src, 0, b'*'), None);
    }

    #[test]
    fn colon_extent_is_opaque_to_at_form_groups() {
        // A `}` inside a props string must not clip the colon body; the depth-0 `}` after it does.
        let src = "@a: @f[x: \"}\"] y} tail";
        let colon_end = src.find(':').unwrap() as u32 + 1;
        let (start, end) = colon_block_extent(src, colon_end, 0, true);
        assert_eq!(&src[start as usize..end as usize], "@f[x: \"}\"] y");
    }

    #[test]
    fn math_boundary_walks_interps_and_close() {
        let src = "$a_@i + @(f(x)) @@ b$ tail";
        let (e, b) = math_boundary(src, 1, false);
        assert_eq!(&src[1..e as usize], "a_");
        let MathBoundary::InterpName { name_end } = b else { panic!("expected @i interp") };
        assert_eq!(&src[e as usize + 1..name_end as usize], "i");

        let (e, b) = math_boundary(src, name_end, false);
        assert_eq!(&src[name_end as usize..e as usize], " + ");
        assert!(matches!(b, MathBoundary::InterpParen));

        // `@(f(x))` is the parser's; resume after it (offset 15). `@@` yields two literal `@`s
        // (each `@` has no interpolation head), then ` b` and the closing `$`.
        let (e, b) = math_boundary(src, 15, false);
        assert_eq!(&src[15..e as usize], " ");
        assert!(matches!(b, MathBoundary::LiteralAt));
        let (e, b) = math_boundary(src, e + 1, false);
        assert_eq!(e, 17);
        assert!(matches!(b, MathBoundary::LiteralAt));
        let (e, b) = math_boundary(src, e + 1, false);
        assert_eq!(&src[18..e as usize], " b");
        assert!(matches!(b, MathBoundary::Close { after: 21 }));

        // A `\$` stays raw; the unescaped `$` closes.
        let src = r"$a\$b$ t";
        let (e, b) = math_boundary(src, 1, false);
        assert_eq!(&src[1..e as usize], r"a\$b");
        assert!(matches!(b, MathBoundary::Close { after } if after == e + 1));

        // Display math: a single `$` is literal; `$$` closes.
        let src = "$$a$b$$";
        let (e, b) = math_boundary(src, 2, true);
        assert_eq!(&src[2..e as usize], "a$b");
        assert!(matches!(b, MathBoundary::Close { after } if after == e + 2));

        // Unterminated → Eof.
        assert!(matches!(math_boundary("$abc", 1, false), (4, MathBoundary::Eof)));
    }

    #[test]
    fn verbatim_boundary_close_armed_eof() {
        // Close, with the single trailing newline dropped from the run.
        let src = "raw\n}| t";
        let (run_end, b) = verbatim_boundary(src, 0);
        assert_eq!(&src[0..run_end as usize], "raw");
        assert!(matches!(b, VerbatimBoundary::Close { after: 6 }));

        // Armed `|@`: the run ends before the `|`, the `@` position is reported.
        let src = "ab|@x{y}}|";
        let (run_end, b) = verbatim_boundary(src, 0);
        assert_eq!(&src[0..run_end as usize], "ab");
        assert!(matches!(b, VerbatimBoundary::ArmedAt { at: 3 }));

        // Unterminated → Eof with the full tail as the run.
        let (run_end, b) = verbatim_boundary("abc", 0);
        assert_eq!(run_end, 3);
        assert!(matches!(b, VerbatimBoundary::Eof));
    }

    #[test]
    fn line_classifiers() {
        // statement lines
        assert!(is_statement_line("  % const x = 1", 0));
        assert!(!is_statement_line("  x % y", 0));
        assert_eq!(statement_kind("  % f()\n", 0), Some((3, false)));
        assert_eq!(statement_kind("%%%\nbody\n", 0), Some((4, true)));
        assert_eq!(statement_kind("%%% x\n", 0), Some((1, false))); // content after run → statement
        assert_eq!(statement_kind("x\n", 0), None);
        assert!(percent_line_is_empty("%   \nx", 1));
        assert!(percent_line_is_empty("% // note\nx", 1));
        assert!(!percent_line_is_empty("% f()\n", 1));

        // headings
        assert_eq!(heading_at("### Sub\n", 0), Some((3, 4, 7)));
        assert_eq!(heading_at("####### seven\n", 0), None);
        assert_eq!(heading_at("#nospace\n", 0), None);

        // list markers
        let m = list_marker_at("  - item\n", 0).unwrap();
        assert!(!m.ordered);
        assert_eq!((m.indent, m.offset, m.body_col), (2, 2, 4));
        let m = list_marker_at("12. item\n", 0).unwrap();
        assert!(m.ordered);
        assert_eq!(m.body_col, 4);
        assert!(list_marker_at("-nospace\n", 0).is_none());

        // `|` prop lines
        assert_eq!(colon_prop_line_at("  | x: 1\n", 0), Some(3));
        assert_eq!(colon_prop_line_at("  x | y\n", 0), None);
    }
}
