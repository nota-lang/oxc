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
//! typed tokens and AST construction. Line-start classifiers are `lazy-regex` patterns over the
//! line slice; the extent walkers step a shared [`Scan`] byte cursor, so each scan reads as its
//! grammar rule rather than index arithmetic.

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
    || b == b'|'
    // Doc-state sugar openers (notation.md §Doc-state references): `<label>`, `&ref`, `[^mark]`.
    // Each is validated at
    // the sigil in `next_nota_child` (left-guard / digraph shape); a non-opener stays 1-byte text.
    || b == b'<'
    || b == b'&'
    || b == b'['
    // Comment openers (`//` line, `/* … */` block — Typst/C style); a lone `/` stays 1-byte text.
    || b == b'/'
    // Strikethrough `~~` (two-byte emphasis marker); a lone `~` stays 1-byte text.
    || b == b'~'
    // Image opener `![` (notation.md §Links); a lone `!` stays 1-byte text.
    || b == b'!');

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
            // Doc-state sugar openers. A marker token only at a valid opener (the
            // left-boundary guard for `<`/`&`, the `[^`+ident digraph for `[`); otherwise a 1-byte
            // text token. The parser (`parse_*_sugar`) resolves the terminator and marker-vs-literal.
            Some(b'<') => {
                let kind = if label_can_open(self.source.whole(), start) {
                    Kind::LAngle
                } else {
                    Kind::MarkupText
                };
                self.consume_char();
                return self.finish_re_lex(kind);
            }
            Some(b'&') => {
                let kind = if ref_can_open(self.source.whole(), start) {
                    Kind::Amp
                } else {
                    Kind::MarkupText
                };
                self.consume_char();
                return self.finish_re_lex(kind);
            }
            // A `[` is always a typed token (unless escaped): the parser resolves which of the
            // three bracket sugars applies — footnote `[^mark]`, link `[text](url)` — or falls
            // back to a literal `[` (notation.md §Links).
            Some(b'[') => {
                let kind = if is_escaped(self.source.whole(), start) {
                    Kind::MarkupText
                } else {
                    Kind::LBrack
                };
                self.consume_char();
                return self.finish_re_lex(kind);
            }
            // Image opener `![` (notation.md §Links): a marker token only at the digraph; a lone
            // `!` stays 1-byte text. The parser validates the full `![alt](src)` shape.
            Some(b'!') => {
                let kind = if !is_escaped(self.source.whole(), start)
                    && byte_at(self.source.whole(), start + 1) == Some(b'[')
                {
                    Kind::Bang
                } else {
                    Kind::MarkupText
                };
                self.consume_char();
                return self.finish_re_lex(kind);
            }
            // Strikethrough `~~`: a 2-byte marker token only at a valid opener (the emphasis
            // word-boundary rule judged across the pair); otherwise the `~` is a 1-byte text
            // token. The matching close is resolved by the parser via `find_strike_close`.
            Some(b'~') => {
                if strike_can_open(self.source.whole(), start) {
                    self.consume_char();
                    self.consume_char();
                    return self.finish_re_lex(Kind::Tilde);
                }
                self.consume_char();
                return self.finish_re_lex(Kind::MarkupText);
            }
            // Comments (Typst/C style): a marker token only at a valid opener (an unescaped `/`
            // directly followed by `/` or `*`); otherwise the `/` is a 1-byte text token. The
            // parser scans the extent ([`lex_comment`]) — a comment is trivia, never a child.
            Some(b'/') => {
                let kind = if comment_can_open(self.source.whole(), start) {
                    Kind::Slash
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

/// A byte cursor over the raw source — the shared stepping machinery of the extent walkers below.
///
/// Methods are grammar-shaped (`eat_run`, `skip_while`, `eat_keyword`, …) so a scan reads as its
/// rule rather than index arithmetic. The cursor is `Copy`: a scan looks ahead by stepping a
/// throwaway copy. Reads go through `get` (an escape's `advance(2)` may overshoot the end by one
/// byte); [`Scan::pos`] clamps to the source end.
#[derive(Clone, Copy)]
struct Scan<'a> {
    source: &'a str,
    i: usize,
}

impl<'a> Scan<'a> {
    fn new(source: &'a str, at: u32) -> Self {
        Self { source, i: at as usize }
    }

    fn bytes(self) -> &'a [u8] {
        self.source.as_bytes()
    }

    /// The cursor's byte offset, clamped to the source end.
    fn pos(self) -> u32 {
        self.i.min(self.source.len()) as u32
    }

    fn is_eof(self) -> bool {
        self.i >= self.source.len()
    }

    fn peek(self) -> Option<u8> {
        self.bytes().get(self.i).copied()
    }

    fn peek_at(self, k: usize) -> Option<u8> {
        self.bytes().get(self.i + k).copied()
    }

    /// Are the next two bytes exactly `a`, `b`?
    fn at2(self, a: u8, b: u8) -> bool {
        self.peek() == Some(a) && self.peek_at(1) == Some(b)
    }

    fn bump(&mut self) {
        self.i += 1;
    }

    fn advance(&mut self, n: usize) {
        self.i += n;
    }

    fn goto(&mut self, pos: u32) {
        self.i = pos as usize;
    }

    /// Consume the run of `b`; returns its length.
    fn eat_run(&mut self, b: u8) -> usize {
        let start = self.i;
        while self.peek() == Some(b) {
            self.i += 1;
        }
        self.i - start
    }

    /// Consume bytes while `pred` holds.
    fn skip_while(&mut self, pred: impl Fn(u8) -> bool) {
        while self.peek().is_some_and(&pred) {
            self.i += 1;
        }
    }

    /// Consume spaces/tabs.
    fn skip_inline_ws(&mut self) {
        self.skip_while(|b| matches!(b, b' ' | b'\t'));
    }

    /// Jump to the next occurrence of `b` at/after the cursor (a `memchr` jump); `false` leaves
    /// the cursor at the source end.
    fn find(&mut self, b: u8) -> bool {
        let at = self.i.min(self.source.len());
        if let Some(k) = memchr::memchr(b, &self.bytes()[at..]) {
            self.i = at + k;
            true
        } else {
            self.i = self.source.len();
            false
        }
    }

    /// Consume `kw` if the source continues with it followed by a word boundary (so `else`
    /// matches but `elsewhere` does not).
    fn eat_keyword(&mut self, kw: &[u8]) -> bool {
        let Some(rest) = self.bytes().get(self.i..) else { return false };
        if !rest.starts_with(kw) {
            return false;
        }
        let boundary = match rest.get(kw.len()) {
            Some(&b) => !(b.is_ascii_alphanumeric() || matches!(b, b'_' | b'$') || b >= 0x80),
            None => true,
        };
        if boundary {
            self.i += kw.len();
        }
        boundary
    }
}

/// The end of the run of `byte` starting at `off` (for literal sigil-run fallbacks).
pub fn sigil_run_end(source: &str, off: u32, byte: u8) -> u32 {
    let mut s = Scan::new(source, off);
    s.eat_run(byte);
    s.pos()
}

/// `(content_offset, is_blank)` for the line starting at `line_start`: the offset of its first
/// non-inline-whitespace byte, and whether the line has no content.
fn line_probe(source: &str, line_start: u32) -> (u32, bool) {
    let mut s = Scan::new(source, line_start);
    s.skip_inline_ws();
    (s.pos(), s.peek().is_none_or(|b| b == b'\n'))
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
    let mut s = Scan::new(source, start as u32);
    s.skip_inline_ws();
    s.pos() as usize - start
}

/// The end of an indentation-scoped block: starting at line `line_start`, consume lines that are
/// blank or indented strictly past `min_indent`; return the first line at/under it (or EOF).
/// This is the shared block-extent rule for list-item bodies and colon-sugar continuations.
fn indented_block_end(source: &str, mut line_start: u32, min_indent: u32) -> u32 {
    while (line_start as usize) < source.len() {
        let (content, is_blank) = line_probe(source, line_start);
        if !is_blank && content - line_start <= min_indent {
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
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut s = Scan::new(source, at);
    let mut consumed = false;
    while s.peek() == Some(b'-') && s.peek_at(1).is_some_and(is_ident) {
        s.bump();
        s.skip_while(is_ident);
        consumed = true;
    }
    consumed.then_some(s.pos())
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

/// Is `at` a "line start modulo whitespace" inside a markup frame whose body content begins at
/// `frame_start` (notation.md §Markup sugar)? Walk back from `at` over spaces/tabs — never below `frame_start`,
/// which bounds the frame's own body — and report whether the landing sits at the frame's body
/// start, at file offset 0, or immediately after a `\n`. This is the position half of the
/// positional colon-sugar gate: `@head:` is an element trigger only where this holds (and the top
/// region is markup). A markup body's own start counts as a line start, so `@a` in `@a: @b: c`
/// (the inner form sits at its enclosing colon body's start) and `@p{  @a: b}` both qualify.
pub fn at_line_start_in_frame(source: &str, at: u32, frame_start: u32) -> bool {
    let bytes = source.as_bytes();
    let mut pos = at;
    while pos > frame_start && matches!(bytes.get(pos as usize - 1), Some(b' ' | b'\t')) {
        pos -= 1;
    }
    pos == frame_start || pos == 0 || bytes.get(pos as usize - 1) == Some(&b'\n')
}

// ================================================================================================
// Doc-state sugar (notation.md §Doc-state references): `<label>` / `&ref` / `[^mark]` /
// line-start `[^label]: body`
// ================================================================================================

/// The doc-state sugar **label** charset: **Typst minus period**, ASCII-only
/// (notation.md §Doc-state references). Start `[A-Za-z0-9_]`: digits are legal at a label's *start* (`[^1]` fires,
/// Markdown-style), unlike a JS identifier. The element forms remain charset-free (`@Label[id:
/// "π.α"]{}` takes any string) — only the sugar is restricted.
fn is_docstate_label_start(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// The doc-state sugar label *continue* charset: `[A-Za-z0-9_:-]` — `-` and `:` join
/// (kebab/namespaced labels: `<sec-intro>`, `<ns:x>`), but `.` does NOT (so `&sec.` never glues the
/// trailing dot). `$` and non-ASCII are not label chars (a Unicode-letter label stays literal).
fn is_docstate_label_part(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b':' | b'-')
}

/// Is the byte at `off` a doc-state label-*start* char (`[A-Za-z0-9_]`)? The shape half of the
/// `<`/`&`/`[^` opener checks. ASCII-only: a non-ASCII lead byte (`≥0x80`) is not a label char, so a
/// Unicode-letter label never opens the sugar.
fn is_docstate_start_at(source: &str, off: u32) -> bool {
    byte_at(source, off).is_some_and(is_docstate_label_start)
}

/// Lexer opener check for `<label>`: unescaped and directly followed by a doc-state label-start char
/// — the *shape* half only. The left-boundary guard needs the enclosing frame's body start, which
/// only the parser knows ([`docstate_left_guard`] + the frame-start check there).
pub fn label_can_open(source: &str, off: u32) -> bool {
    !is_escaped(source, off) && is_docstate_start_at(source, off + 1)
}

/// Lexer opener check for `&ref` (shape half; see [`label_can_open`]).
pub fn ref_can_open(source: &str, off: u32) -> bool {
    !is_escaped(source, off) && is_docstate_start_at(source, off + 1)
}

/// The left-boundary guard on `<` and `&` (notation.md §Doc-state references): the sigil fires iff preceded by
/// start of source, whitespace, or opening punctuation (`(`/`[`/`{`/double/single quote) — so
/// `Vec<T>`, `R&D`, `a<b`, `a&b` stay literal prose. Start-of-*body* also fires, but that is the
/// parser's frame-start check (raw bytes cannot see a body boundary — `*<x>*`).
pub fn docstate_left_guard(source: &str, off: u32) -> bool {
    match (off as usize).checked_sub(1).and_then(|p| source.as_bytes().get(p)) {
        None => true, // start of source
        Some(&b) => b.is_ascii_whitespace() || matches!(b, b'(' | b'[' | b'{' | b'"' | b'\''),
    }
}

/// The exclusive end of a doc-state label starting at `start`, scanning within `limit` (a bounded
/// frame's clip — a match may not reach past the frame); `None` if `start` is at/past `limit` or
/// not a label-start char. The charset is **Typst minus period**
/// (notation.md §Doc-state references): [`is_docstate_label_start`] then [`is_docstate_label_part`], so `-`/`:` join but `.`
/// does not (`&sec.` → `sec` + a literal `.`) and `$`/non-ASCII are not label chars (a
/// Unicode-letter label is literal). ASCII-only ⇒ every char is one byte, so a continuation byte
/// joins iff it sits strictly before `limit`.
fn docstate_ident_end(source: &str, start: u32, limit: u32) -> Option<u32> {
    if start >= limit || !byte_at(source, start).is_some_and(is_docstate_label_start) {
        return None;
    }
    let mut end = start + 1;
    while end < limit && byte_at(source, end).is_some_and(is_docstate_label_part) {
        end += 1;
    }
    Some(end)
}

/// `<label>` at `lt_off`: the label's span, requiring the `>` close within `limit`. The ident
/// charset excludes `\n`, so "closes on its opening line" (matching the inline-span line clamp)
/// holds by construction.
/// `None` → the `<` is literal text.
pub fn label_sugar_at(source: &str, lt_off: u32, limit: u32) -> Option<Span> {
    let start = lt_off + 1;
    let end = docstate_ident_end(source, start, limit)?;
    (end < limit && byte_at(source, end) == Some(b'>')).then(|| Span::new(start, end))
}

/// `&ref` at `amp_off`: the ref's span — it simply ends at the first non-ident byte (or `limit`).
/// `None` → the `&` is literal text.
pub fn ref_sugar_at(source: &str, amp_off: u32, limit: u32) -> Option<Span> {
    docstate_ident_end(source, amp_off + 1, limit).map(|end| Span::new(amp_off + 1, end))
}

/// `[^mark]` at `lbrack_off`: the mark's span, requiring the `[^` digraph and the `]` within
/// `limit`. `None` → the `[` is not a footnote opener (the parser tries the link shape next).
/// (The `[^ident]:` footnote-*text* split is the parser's: it needs the positional line-start
/// gate — notation.md §Colon & block sugar.)
pub fn footnote_sugar_at(source: &str, lbrack_off: u32, limit: u32) -> Option<Span> {
    if byte_at(source, lbrack_off + 1) != Some(b'^') {
        return None;
    }
    let start = lbrack_off + 2;
    let end = docstate_ident_end(source, start, limit)?;
    (end < limit && byte_at(source, end) == Some(b']')).then(|| Span::new(start, end))
}

// ================================================================================================
// Links `[text](url)` and images `![alt](src)` (notation.md §Links)
// ================================================================================================

/// The spans of a scanned `[text](url)` shape ([`lex_link_span`]).
pub struct LinkSpans {
    /// The text extent (inside `[…]`) — a bounded markup body for a link, plain text for an
    /// image's alt.
    pub text: Span,
    /// The url extent (inside `(…)`) — a raw slice; trimming and `\<c>` cooking happen at
    /// lowering.
    pub url: Span,
    /// One past the closing `)`.
    pub resume: u32,
}

/// Scan a `[text](url)` shape whose `[` sits at `lbrack`, within `limit` (a bounded frame's clip
/// — a match may not reach past the frame). The whole shape must close on its opening line (the
/// inline-span line clamp). The text scan pairs nested `[`/`]`, steps over escapes, `@`-forms,
/// raw spans, and comments (their `]` is not structure), and fails at a depth-0 `}` (the
/// enclosing body closes first) or a `//` comment (which claims the rest of the line). The `(`
/// must be glued to the `]`; the url scan pairs nested `(`/`)` and `{`/`}` and steps over
/// escapes only (a url is raw). `None` → the `[` is not a link opener.
pub fn lex_link_span(source: &str, lbrack: u32, limit: u32) -> Option<LinkSpans> {
    let bound = line_content_end(source, lbrack).min(limit).min(source.len() as u32);

    // --- text: the depth-0 `]` on the opening line ---
    let mut s = Scan::new(source, lbrack + 1);
    let mut bracket = 0u32;
    let mut brace = 0i32;
    let text_end = loop {
        if s.pos() >= bound {
            return None;
        }
        match s.peek()? {
            b'\\' => s.advance(2),
            b'[' => {
                bracket += 1;
                s.bump();
            }
            b']' if bracket == 0 => break s.pos(),
            b']' => {
                bracket -= 1;
                s.bump();
            }
            b'{' => {
                brace += 1;
                s.bump();
            }
            b'}' if brace == 0 => return None, // the enclosing body closes first
            b'}' => {
                brace -= 1;
                s.bump();
            }
            b'`' | b'$' => s.skip_raw_span(),
            b'|' if s.peek_at(1) == Some(b'{') => s.skip_raw_span(),
            b'@' => s.skip_at_form(),
            b'/' if s.peek_at(1) == Some(b'/') => return None,
            b'/' if s.peek_at(1) == Some(b'*') => s.skip_markup_block_comment(),
            _ => s.bump(),
        }
    };

    // --- the glued `(`, then the url's depth-0 `)` on the same line ---
    if text_end + 1 >= bound || byte_at(source, text_end + 1) != Some(b'(') {
        return None;
    }
    let mut s = Scan::new(source, text_end + 2);
    let mut paren = 0u32;
    let mut brace = 0i32;
    let url_end = loop {
        if s.pos() >= bound {
            return None;
        }
        match s.peek()? {
            b'\\' => s.advance(2),
            b'(' => {
                paren += 1;
                s.bump();
            }
            b')' if paren == 0 => break s.pos(),
            b')' => {
                paren -= 1;
                s.bump();
            }
            b'{' => {
                brace += 1;
                s.bump();
            }
            b'}' if brace == 0 => return None, // a body closer is never url content (escape it)
            b'}' => {
                brace -= 1;
                s.bump();
            }
            _ => s.bump(),
        }
    };
    Some(LinkSpans {
        text: Span::new(lbrack + 1, text_end),
        url: Span::new(text_end + 2, url_end),
        resume: url_end + 1,
    })
}

// ================================================================================================
// Attrs groups: a trailing bare `[props]` in markup text position (notation.md §Attrs)
// ================================================================================================

/// Detect a **trailing bare attrs group** `[k: v, …]` at `lbrack`. Two gates keep prose honest:
///
/// 1. **First-entry shape**: the interior must open (modulo whitespace) with `...spread`, a
///    quoted key, or `ident` glued to a `:` — so `see [1]` and `[just words]` stay literal.
/// 2. **Trailing position**: after the group's `]` (balanced, string/comment-aware), only inline
///    whitespace may follow on the closing line up to the line end / `limit` — or, when
///    `closer_ok`, the enclosing body's `}`.
///
/// The whole group must sit within `limit` (a bounded frame's clip). Returns the offset one past
/// the `]`, or `None` (not an attrs group — the `[` falls through to literal text).
pub fn attrs_group_at(source: &str, lbrack: u32, limit: u32, closer_ok: bool) -> Option<u32> {
    // Gate 1: the first-entry shape.
    let mut s = Scan::new(source, lbrack + 1);
    s.skip_inline_ws();
    let gate_ok = match s.peek()? {
        b'.' => s.peek_at(1) == Some(b'.') && s.peek_at(2) == Some(b'.'),
        b'"' | b'\'' => true, // quoted key (`["data-x": v]`) — the props parse validates the `:`
        b if b.is_ascii_alphabetic() || b == b'_' || b == b'$' => {
            s.skip_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'$'));
            s.skip_inline_ws();
            s.peek() == Some(b':')
        }
        _ => false,
    };
    if !gate_ok {
        return None;
    }
    // The balanced group extent (strings/comments opaque), clamped to the frame.
    let mut g = Scan::new(source, lbrack);
    g.skip_balanced();
    let after = g.pos();
    if after <= lbrack + 1 || byte_at(source, after - 1) != Some(b']') || after > limit {
        return None;
    }
    // Gate 2: trailing on the group's closing line.
    let line_end = line_content_end(source, after).min(limit);
    let mut t = Scan::new(source, after);
    t.skip_inline_ws();
    if t.pos() >= line_end || (closer_ok && t.peek() == Some(b'}')) { Some(after) } else { None }
}

// ================================================================================================
// Comments (`//` line, `/* … */` block — Typst/C style, in markup text position)
// ================================================================================================

/// Lexer opener check for a markup comment: an unescaped `/` directly followed by `/` or `*`.
pub fn comment_can_open(source: &str, off: u32) -> bool {
    !is_escaped(source, off) && matches!(byte_at(source, off + 1), Some(b'/' | b'*'))
}

/// The result of scanning a markup comment ([`lex_comment`]).
pub struct CommentScan {
    /// One past the comment's last byte: for `//`, the line's content end (the `\n` excluded);
    /// for `/* */`, one past the closing `*/`.
    pub end: u32,
    /// `true` for a `/* … */` block comment.
    pub block: bool,
    /// `false` when a block comment ran into `limit` with unbalanced `/*` depth.
    pub terminated: bool,
}

/// Scan the comment opened at `off` (a valid opener per [`comment_can_open`]), within `limit` (a
/// bounded frame's clip / the clamped scan view's end). A `//` comment runs to its line's content
/// end; a `/* … */` block comment runs to the matching `*/` — **nesting counts**, Typst-style
/// (`/* a /* b */ c */` is one comment). An unterminated block comment reports
/// `terminated: false` with `end == limit`.
pub fn lex_comment(source: &str, off: u32, limit: u32) -> CommentScan {
    let limit = limit.min(source.len() as u32);
    if byte_at(source, off + 1) == Some(b'/') {
        let end = line_content_end(source, off).min(limit);
        return CommentScan { end, block: false, terminated: true };
    }
    // Typst's nested-block-comment state machine: find the first `*/` that does not close a
    // nested `/*`. `prev` is reset after a match so `/*/` cannot double-count its middle byte.
    let mut s = Scan::new(source, off + 2);
    let mut depth = 1u32;
    let mut prev = 0u8;
    while s.pos() < limit {
        let Some(b) = s.peek() else { break };
        s.bump();
        match (prev, b) {
            (b'*', b'/') => {
                depth -= 1;
                if depth == 0 {
                    return CommentScan { end: s.pos(), block: true, terminated: true };
                }
                prev = 0;
            }
            (b'/', b'*') => {
                depth += 1;
                prev = 0;
            }
            _ => prev = b,
        }
    }
    CommentScan { end: limit, block: true, terminated: false }
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

/// Scan for an `else`/`else if` continuation after an `@if` branch that closed at `close_end`:
/// skip inline whitespace and at most one newline (a blank line breaks the chain), reject an
/// escaped `\else`, then classify what follows `else` (`if` → else-if, `{` → else-block).
pub fn else_peek(source: &str, close_end: u32) -> ElsePeek {
    let mut s = Scan::new(source, close_end);
    let mut newlines = 0u32;
    loop {
        match s.peek() {
            Some(b' ' | b'\t' | b'\r') => s.bump(),
            Some(b'\n') => {
                newlines += 1;
                if newlines >= 2 {
                    return ElsePeek::None;
                }
                s.bump();
            }
            _ => break,
        }
    }
    if s.peek() == Some(b'\\') || !s.eat_keyword(b"else") {
        return ElsePeek::None;
    }
    s.skip_while(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'));
    let after_else = s.pos();
    if s.eat_keyword(b"if") {
        ElsePeek::ElseIf { if_offset: after_else }
    } else if s.peek() == Some(b'{') {
        ElsePeek::Else { brace_offset: after_else }
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

/// The exclusive bound of the `%` statement region whose JS begins at `content`: the start of the
/// next line-leading `%` line (a statement delimiter the JS lexer would otherwise read as
/// modulo), the start of the first **blank** line (a blank line always ends a `%` statement —
/// with the lexer clamped there, ASI applies exactly as at end of input), or the source length.
///
/// Line-level scan by design: a blank line inside a multi-line template literal also bounds the
/// region (the scan cannot see string interiors) — blank-line-bearing code belongs in a `%%%`
/// fence, same as a line-leading `%` inside a template.
pub fn statement_bound(source: &str, content: u32) -> u32 {
    let len = source.len() as u32;
    let mut line = next_line_start(source, content);
    while line < len {
        if is_statement_line(source, line) || line_probe(source, line).1 {
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

/// Detect a `---` thematic break at `line_start`: a run of 3+ `-` (leading indentation
/// tolerated) whose tail up to `line_end` — the caller's clipped line extent (content end ∧ brace
/// clip ∧ bounded end) — is whitespace-only. Returns the `-` run's span, or `None`. (A `- ` list
/// marker never matches: it has a space after one `-`; `---` never matches a list marker for the
/// same reason.)
pub fn thematic_break_at(source: &str, line_start: u32, line_end: u32) -> Option<Span> {
    let mut s = Scan::new(source, line_start);
    s.skip_inline_ws();
    let run_start = s.pos();
    let run = s.eat_run(b'-');
    let run_end = s.pos();
    s.skip_while(|b| matches!(b, b' ' | b'\t' | b'\r'));
    (run >= 3 && run_end <= line_end && s.pos() >= line_end).then(|| Span::new(run_start, run_end))
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

/// Scan the line from `from` for a **depth-0 `}`**: `\`-escaped bytes are skipped, an `@`-form's
/// head + `(…)`/`[…]` groups are opaque ([`Scan::skip_at_form`]) — a `}` inside embedded-JS props
/// cannot match — and balanced `{…}` on the line is tracked. Returns the `}`'s offset, or `None`
/// when the line has no depth-0 `}`. This is the brace clip shared by colon sugar's first line
/// and by line-start sugar armed inside a braced body (`@{- item}` — the item's extent must not
/// eat the body's closer).
pub fn brace_clip_on_line(source: &str, from: u32) -> Option<u32> {
    let line_end = line_content_end(source, from);
    let mut s = Scan::new(source, from);
    let mut depth = 0i32;
    loop {
        match s.peek() {
            None | Some(b'\n') => return None,
            Some(b'\\') => s.advance(2), // skip the escaped byte
            Some(b'@') => s.skip_at_form(),
            // A `}` inside a comment is not structure: a `//` comment claims the rest of the
            // line; a block comment that crosses the line end leaves no depth-0 `}` on it.
            Some(b'/') if s.peek_at(1) == Some(b'/') => return None,
            Some(b'/') if s.peek_at(1) == Some(b'*') => {
                s.skip_markup_block_comment();
                if s.pos() > line_end {
                    return None;
                }
            }
            Some(b'{') => {
                depth += 1;
                s.bump();
            }
            Some(b'}') if depth == 0 => return Some(s.pos()),
            Some(b'}') => {
                depth = (depth - 1).max(0);
                s.bump();
            }
            _ => s.bump(),
        }
    }
}

/// Compute the source extent `[start, end)` of a `@head:` colon-sugar body: the rest of the
/// `@head:` line (after inline whitespace) plus following lines indented strictly past
/// `head_indent`. When `clip_at_brace`, a depth-0 `}` (closing an enclosing `{…}` body) ends the
/// body on the first line ([`brace_clip_on_line`]).
pub fn colon_block_extent(
    source: &str,
    colon_end: u32,
    head_indent: usize,
    clip_at_brace: bool,
) -> (u32, u32) {
    let mut s = Scan::new(source, colon_end);
    s.skip_inline_ws();
    let start = s.pos();
    if clip_at_brace && let Some(clip) = brace_clip_on_line(source, colon_end) {
        return (start, clip);
    }
    let first_line_end = next_line_start(source, colon_end);
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

/// Is the `len`-byte marker run at `marker_off` significant (unescaped, not intra-word — the
/// word-boundary chars are the ones just outside the whole run)?
fn is_marker(source: &str, marker_off: u32, len: u32) -> bool {
    !(is_escaped(source, marker_off)
        || is_wordy(char_before(source, marker_off)) && is_wordy(char_at(source, marker_off + len)))
}

/// Can the `len`-byte marker run at `off` **close** its span? A marker immediately preceded by
/// content (a non-whitespace byte), so `foo *` / `foo ~~` do not close.
fn can_close(source: &str, off: u32, len: u32) -> bool {
    if !is_marker(source, off, len) {
        return false;
    }
    match (off as usize).checked_sub(1).and_then(|p| source.as_bytes().get(p)) {
        Some(&b) => !b.is_ascii_whitespace(),
        None => false,
    }
}

/// Can a `~~` at byte `off` **open** a strikethrough span (notation.md §Markup sugar)? Mirrors
/// [`emphasis_can_open`] with a two-byte marker: unescaped, the `~~` digraph, not intra-word (the
/// word-boundary rule judged across the pair), and immediately followed by content — a
/// non-whitespace byte that is not another `~` (so runs `~~~` and `a~~b` stay literal).
pub fn strike_can_open(source: &str, off: u32) -> bool {
    if byte_at(source, off + 1) != Some(b'~') || is_escaped(source, off) {
        return false;
    }
    if is_wordy(char_before(source, off)) && is_wordy(char_at(source, off + 2)) {
        return false;
    }
    matches!(byte_at(source, off + 2), Some(b) if !b.is_ascii_whitespace() && b != b'~')
}

/// The embedded-JS / raw-span skips: extent walkers step *over* these regions so their contents
/// cannot perturb the surrounding scan (a `*` in a string cannot close an emphasis, a `}` in a
/// props string cannot clip a colon body).
impl Scan<'_> {
    /// Skip a JS string or template literal whose opening quote is next, honoring `\`-escapes;
    /// leaves the cursor just past the closing quote. An unterminated `'`/`"` stops *at* the
    /// newline (the scan resumes there); a template runs to its closing backtick (newlines
    /// allowed, `${…}` contents opaque); unterminated at EOF stops at the source end.
    fn skip_js_string(&mut self) {
        let quote = self.peek();
        self.bump();
        loop {
            match self.peek() {
                None => return,
                Some(b'\\') => self.advance(2),
                Some(b'\n') if quote != Some(b'`') => return, // unterminated: stop at the newline
                b if b == quote => {
                    self.bump();
                    return;
                }
                _ => self.bump(),
            }
        }
    }

    /// Skip a balanced bracket group (`(…)`/`[…]`/`{…}`, nesting all three) whose opener is next;
    /// leaves the cursor just past the matching closer, or one byte in if unterminated. The group
    /// is embedded JS, so string/template and comment contents are skipped — a bracket or emphasis
    /// marker inside `"…"`/`` `…` ``/`/*…*/` cannot unbalance it. (Regex literals are not
    /// recognized; these scans bound heuristic extents — the embedded JS is properly parsed
    /// afterwards.)
    fn skip_balanced(&mut self) {
        let entry = self.i;
        let mut depth = 0u32;
        while let Some(b) = self.peek() {
            match b {
                b'(' | b'[' | b'{' => {
                    depth += 1;
                    self.bump();
                }
                b')' | b']' | b'}' => {
                    depth -= 1;
                    self.bump();
                    if depth == 0 {
                        return;
                    }
                }
                b'"' | b'\'' | b'`' => self.skip_js_string(),
                b'/' if self.peek_at(1) == Some(b'/') => {
                    self.goto(line_content_end(self.source, self.pos()));
                }
                b'/' if self.peek_at(1) == Some(b'*') => {
                    self.i = match memchr::memmem::find(&self.bytes()[self.i + 2..], b"*/") {
                        Some(k) => self.i + 2 + k + 2,
                        None => self.source.len(),
                    };
                }
                _ => self.bump(),
            }
        }
        self.i = entry + 1; // unterminated: the opener is a single literal byte
    }

    /// Skip an `@`-form whose `@` is next: past the head and any adjacent `(…)`/`[…]` groups — so
    /// a `*`/`_` inside an embedded expression cannot close an emphasis, and a stray bracket
    /// inside that JS cannot perturb the caller's brace depth. A trailing `{…}` markup body is
    /// left to the caller's depth-tracked scan.
    fn skip_at_form(&mut self) {
        self.bump(); // the `@`
        self.skip_while(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'_' | b'$' | b'.') || b >= 0x80
        });
        while matches!(self.peek(), Some(b'(' | b'[')) {
            self.skip_balanced();
        }
    }

    /// Skip a markup `/* … */` block comment whose `/*` is next (nesting honored); leaves the
    /// cursor just past the closing `*/`, or at the source end if unterminated. Lets the
    /// surrounding scan step over commented-out bytes (a `*` or `}` inside a comment is not
    /// structure).
    fn skip_markup_block_comment(&mut self) {
        let scan = lex_comment(self.source, self.pos(), self.source.len() as u32);
        self.goto(scan.end);
    }

    /// Skip a raw span (inline/fenced code, math, or `|{ … }|` verbatim) whose opener byte is
    /// next; leaves the cursor just past its close, or one byte in if it has no valid close (the
    /// opener was literal). Lets emphasis matching step over raw content.
    fn skip_raw_span(&mut self) {
        let entry = self.i;
        match self.peek() {
            Some(b'`') => {
                let fence_len = self.eat_run(b'`');
                match find_backtick_close(self.source, self.pos(), fence_len) {
                    Some(close) => self.i = close as usize + fence_len,
                    None => self.i = entry + 1,
                }
            }
            // Math shares the code extent shape (inline `≥N`-close / display fence); delegate to the
            // one dollar scan so emphasis steps over exactly the span the reader would build. A
            // multi-line display fence carries the cursor past the emphasis line, killing the span
            // (the caller's post-skip line-bound check) — an inline span never crosses a newline.
            Some(b'$') => match lex_math_span(self.source, self.pos()) {
                MathScan::Math { resume, .. } => self.i = resume as usize,
                MathScan::Literal { .. } => self.i = entry + 1,
            },
            Some(b'|') => {
                self.advance(2); // past `|{`
                loop {
                    if self.is_eof() {
                        self.i = entry + 1;
                        return;
                    }
                    if self.at2(b'}', b'|') {
                        self.advance(2);
                        return;
                    }
                    self.bump();
                }
            }
            _ => self.i = entry + 1,
        }
    }
}

/// Find the matching close marker for an emphasis opened at `open`, or `None` (then the opener is
/// literal). See [`find_marker_close`].
pub fn find_emphasis_close(source: &str, open: u32, marker: u8) -> Option<u32> {
    find_marker_close(source, open, marker, 1)
}

/// Find the matching `~~` close for a strikethrough opened at `open`, or `None` (then both opener
/// bytes are literal). The emphasis scan with a two-byte marker run — see [`find_marker_close`].
pub fn find_strike_close(source: &str, open: u32) -> Option<u32> {
    find_marker_close(source, open, b'~', 2)
}

/// Scan forward from a `len`-byte marker run opened at `open` for the next valid close, bounded
/// by the span's *scope*: the end of the opening line (an inline span never crosses a newline —
/// the CommonMark-style clamp), the `}` closing the enclosing body, or EOF. Balanced `{…}`, raw
/// spans, comments, and `@`-forms are skipped so their inner marker bytes cannot close; a skip
/// that crosses the line end kills the span too.
fn find_marker_close(source: &str, open: u32, marker: u8, len: u32) -> Option<u32> {
    let bound = line_content_end(source, open);
    let mut s = Scan::new(source, open + len);
    let mut depth = 0i32;
    while let Some(b) = s.peek() {
        if s.pos() >= bound {
            return None; // line end (or a skip crossed it): the opener is literal
        }
        match b {
            b'\\' => s.advance(2), // skip the escaped char (so `\*` cannot close)
            b'{' => {
                depth += 1;
                s.bump();
            }
            b'}' => {
                if depth == 0 {
                    return None; // enclosing body closes before a matching marker
                }
                depth -= 1;
                s.bump();
            }
            b'`' | b'$' => s.skip_raw_span(),
            b'|' if s.peek_at(1) == Some(b'{') => s.skip_raw_span(),
            b'@' => s.skip_at_form(),
            // A `//` comment claims the rest of the line — no close can follow on it; a block
            // comment is skipped whole (one crossing the line end kills the span via the bound).
            b'/' if s.peek_at(1) == Some(b'/') => return None,
            b'/' if s.peek_at(1) == Some(b'*') => s.skip_markup_block_comment(),
            // A link/image extent is opaque — a marker byte inside its text or url cannot close
            // (links bind tighter than emphasis, CommonMark-style).
            b'[' => match lex_link_span(source, s.pos(), bound) {
                Some(link) => s.goto(link.resume),
                None => s.bump(),
            },
            b'!' if s.peek_at(1) == Some(b'[') => match lex_link_span(source, s.pos() + 1, bound) {
                Some(link) => s.goto(link.resume),
                None => s.bump(),
            },
            _ if b == marker && depth == 0 => {
                let run_ok = len == 1 || s.peek_at(1) == Some(marker);
                if run_ok && s.pos() > open + len && can_close(source, s.pos(), len) {
                    return Some(s.pos());
                }
                s.bump();
            }
            _ => s.bump(),
        }
    }
    None
}

// ================================================================================================
// Code spans (inline `` `…` `` and fenced ```` ```lang⏎…⏎``` ````)
// ================================================================================================

/// The result of scanning a `` ` ``-opened code span ([`lex_code_span`]).
pub enum CodeScan<'a> {
    /// A code span: `span` covers the whole `` `…` ``; `lang` is the block info-string's first
    /// token (inline → `None`); `content` is the raw inner extent `[start, end)` (raw runs
    /// interleaved with `|@`-armed forms — the parser re-scans it with [`armed_boundary`]).
    Code { span: Span, is_block: bool, lang: Option<&'a str>, content: Span, resume: u32 },
    /// Not a valid opener — the backtick run is literal text ending at `resume`.
    Literal { resume: u32 },
}

/// Scan a code span whose opening backtick run starts at `tick_off`. A `≥3` run that is the last
/// non-whitespace on its line (modulo a language tag) opens a *fenced block*; otherwise inline
/// code closed by the next run of `≥ fence_len` backticks on the same line (shorter runs are
/// literal). With no same-line close the run is literal.
pub fn lex_code_span(source: &str, tick_off: u32) -> CodeScan<'_> {
    let mut s = Scan::new(source, tick_off);
    let fence_len = s.eat_run(b'`');
    let content_start = s.pos();

    if fence_len >= 3
        && let Some(code) = scan_fenced_code(source, tick_off, fence_len, content_start)
    {
        return code;
    }

    if let Some(close) = find_backtick_close(source, content_start, fence_len) {
        let resume = close + fence_len as u32;
        return CodeScan::Code {
            span: Span::new(tick_off, resume),
            is_block: false,
            lang: None,
            content: Span::new(content_start, close),
            resume,
        };
    }
    CodeScan::Literal { resume: content_start }
}

/// Find the next run of at least `fence_len` backticks at/after `from` **on the same line** (the
/// offset of its first backtick), or `None`. Inline code never crosses a newline (the
/// CommonMark-style clamp); shorter runs are literal content.
fn find_backtick_close(source: &str, from: u32, fence_len: usize) -> Option<u32> {
    let bound = line_content_end(source, from);
    let mut s = Scan::new(source, from);
    while s.find(b'`') {
        let run_start = s.pos();
        if run_start >= bound {
            return None;
        }
        if s.eat_run(b'`') >= fence_len {
            return Some(run_start);
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
    content_start: u32,
) -> Option<CodeScan<'_>> {
    // The opener line's tail is the info string: it must contain no backticks (⇒ inline run, not
    // a fence) and must end in a newline (a fence needs a body).
    let opener_end = line_content_end(source, content_start);
    let info = &source[content_start as usize..opener_end as usize];
    if info.contains('`') || (opener_end as usize) >= source.len() {
        return None;
    }
    // Language = first token of the info string (```` ```js extra ```` → `js`).
    let lang = info.split_whitespace().next();
    let body_start = opener_end + 1;

    let mut line_start = body_start;
    while (line_start as usize) < source.len() {
        let mut s = Scan::new(source, line_start);
        s.skip_inline_ws();
        if s.eat_run(b'`') >= fence_len {
            // Close fence: body ends before the fence line's `\n` (a non-first line always
            // follows one). Resume right after the backtick run — trailing content (e.g. a `}`
            // closing an enclosing body) is the collector's.
            let code_end = if line_start > body_start { line_start - 1 } else { line_start };
            return Some(CodeScan::Code {
                span: Span::new(tick_off, s.pos()),
                is_block: true,
                lang,
                content: Span::new(body_start, code_end),
                resume: s.pos(),
            });
        }
        line_start = next_line_start(source, line_start);
    }
    // Unterminated fence: code runs to EOF.
    Some(CodeScan::Code {
        span: Span::new(tick_off, source.len() as u32),
        is_block: true,
        lang,
        content: Span::new(body_start, source.len() as u32),
        resume: source.len() as u32,
    })
}

// ================================================================================================
// Math spans (`$…$` inline / `$$⏎…⏎$$` display fence) — structurally mirror code spans
// ================================================================================================

/// The result of scanning a `$`-opened math span ([`lex_math_span`]).
pub enum MathScan {
    /// A math span: `span` covers the whole `$…$` / `$$…$$`; `content` is the raw inner extent
    /// `[start, end)` (raw runs interleaved with `|@`-armed forms). `is_block` is the display fence.
    Math { span: Span, is_block: bool, content: Span, resume: u32 },
    /// Not a valid opener — the `$`-run is literal text ending at `resume`.
    Literal { resume: u32 },
}

/// Scan a math span whose opening `$`-run starts at `dollar_off`. Structurally mirrors
/// [`lex_code_span`]: a `≥2`-dollar run whose opener-line tail is whitespace-only opens a display
/// *fence*; otherwise an inline span closed by the next same-line run of `≥ open_len` dollars
/// (shorter runs are content). With no same-line close the run is literal. Backtick and dollar
/// diverge in exactly one place — the dollar close scan honors TeX's `\<c>` escape (see
/// [`find_dollar_close`]); a nonempty opener tail (dollars or not) forbids the fence (math has no
/// info string), so `$$x$$` is inline run-2, and display math is the standalone-line fence.
pub fn lex_math_span(source: &str, dollar_off: u32) -> MathScan {
    let mut s = Scan::new(source, dollar_off);
    let open_len = s.eat_run(b'$');
    let content_start = s.pos();

    if open_len >= 2
        && let Some(fence) = scan_dollar_fence(source, dollar_off, open_len, content_start)
    {
        return fence;
    }

    if let Some(close) = find_dollar_close(source, content_start, open_len) {
        let resume = close + open_len as u32;
        return MathScan::Math {
            span: Span::new(dollar_off, resume),
            is_block: false,
            content: Span::new(content_start, close),
            resume,
        };
    }
    MathScan::Literal { resume: content_start }
}

/// Find the next run of at least `open_len` dollars at/after `from` **on the same line** (the
/// offset of its first `$`), or `None`. Mirrors [`find_backtick_close`]'s `≥`-rule and line clamp,
/// with the TeX exception: a `\<c>` pair is skipped, so `\$` stays content (LaTeX's own escape) and
/// the backslash is kept in the raw run — backtick scans stay escape-blind.
fn find_dollar_close(source: &str, from: u32, open_len: usize) -> Option<u32> {
    let bound = line_content_end(source, from);
    let mut s = Scan::new(source, from);
    while s.pos() < bound {
        match s.peek() {
            Some(b'\\') => s.advance(2),
            Some(b'$') => {
                let run_start = s.pos();
                if s.eat_run(b'$') >= open_len {
                    return Some(run_start);
                }
            }
            _ => s.bump(),
        }
    }
    None
}

/// Scan a display-math fence opened by an `open_len`-dollar run at `dollar_off` (`content_start`
/// just past it). Mirrors [`scan_fenced_code`]'s shape and its unterminated/EOF behaviour, with two
/// dollar-specific rules: the opener-line tail must be **whitespace-only** (math has no info string
/// — any nonempty tail, dollars or not, means try-inline), and the fence needs a trailing newline
/// (a body follows). The block ends at the first line whose first non-whitespace is a run of
/// `≥ open_len` dollars (or EOF).
fn scan_dollar_fence(
    source: &str,
    dollar_off: u32,
    open_len: usize,
    content_start: u32,
) -> Option<MathScan> {
    let opener_end = line_content_end(source, content_start);
    let tail = &source[content_start as usize..opener_end as usize];
    if !tail.trim().is_empty() || (opener_end as usize) >= source.len() {
        return None;
    }
    let body_start = opener_end + 1;

    let mut line_start = body_start;
    while (line_start as usize) < source.len() {
        let mut s = Scan::new(source, line_start);
        s.skip_inline_ws();
        if s.eat_run(b'$') >= open_len {
            // Close fence: body ends before the fence line's `\n` (a non-first line follows one).
            let code_end = if line_start > body_start { line_start - 1 } else { line_start };
            return Some(MathScan::Math {
                span: Span::new(dollar_off, s.pos()),
                is_block: true,
                content: Span::new(body_start, code_end),
                resume: s.pos(),
            });
        }
        line_start = next_line_start(source, line_start);
    }
    // Unterminated fence: math runs to EOF.
    Some(MathScan::Math {
        span: Span::new(dollar_off, source.len() as u32),
        is_block: true,
        content: Span::new(body_start, source.len() as u32),
        resume: source.len() as u32,
    })
}

// ================================================================================================
// Bounded raw-span content (`|@` arming) + verbatim bodies (`|{ … }|`)
// ================================================================================================

/// A boundary reached while scanning the raw content of a *bounded* raw span (inline/block code or
/// math) for `|@` arming ([`armed_boundary`]).
pub enum ArmedBoundary {
    /// Reached `bound` (the content extent's end) — the raw run ends here.
    Bound,
    /// `|@` — an armed escape: the parser parses one `@`-form at `at` (the `@`).
    ArmedAt { at: u32 },
}

/// Scan the raw content `[from, bound)` of a bounded raw span to the next boundary: an armed `|@`
/// escape, or `bound`. Returns `(run_end, boundary)` where `[from, run_end)` is a raw slice.
/// Generalizes [`verbatim_boundary`] with an explicit end bound — a bounded span's close is its
/// pre-scanned extent end, not a `}|`. There is deliberately no escape for a literal `|@` (as in
/// verbatim); the extent is fixed first, so a `|@` inside it always arms.
pub fn armed_boundary(source: &str, from: u32, bound: u32) -> (u32, ArmedBoundary) {
    let mut s = Scan::new(source, from);
    while s.pos() < bound {
        if s.at2(b'|', b'@') {
            return (s.pos(), ArmedBoundary::ArmedAt { at: s.pos() + 1 });
        }
        s.bump();
    }
    (bound, ArmedBoundary::Bound)
}

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
/// `|@` escape, or EOF. `source` is the reader's clamped scan view (normally the whole source; a
/// prefix bounded to the extent when this verbatim is itself a `|@`-armed form inside a bounded raw
/// span — a `}|` past that extent is then unreachable, so the body reports `Eof` / overruns instead
/// of resuming past the clamp). Returns `(run_end, boundary)` where `[from, run_end)` is the raw
/// slice — for a close, a single trailing newline right before `}|` is dropped (the `}`-newline
/// rule).
pub fn verbatim_boundary(source: &str, from: u32) -> (u32, VerbatimBoundary) {
    let mut s = Scan::new(source, from);
    while !s.is_eof() {
        if s.at2(b'}', b'|') {
            // Drop a single trailing newline right before `}|` (the `}`-newline rule).
            let close = s.pos();
            let run_end = if close > from && byte_at(source, close - 1) == Some(b'\n') {
                close - 1
            } else {
                close
            };
            return (run_end, VerbatimBoundary::Close { after: close + 2 });
        }
        if s.at2(b'|', b'@') {
            return (s.pos(), VerbatimBoundary::ArmedAt { at: s.pos() + 1 });
        }
        s.bump();
    }
    (s.pos(), VerbatimBoundary::Eof)
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

    /// Dollar spans mirror backtick spans: an inline `≥N`-close (with the TeX `\<c>` escape) or a
    /// whitespace-tail display fence.
    #[test]
    fn lex_math_span_inline_and_fence() {
        let content_str = |src: &str, c: Span| src[c.start as usize..c.end as usize].to_string();

        // Inline `$…$`: content is the raw run; resume past the closing `$`.
        let MathScan::Math { content, is_block, resume, .. } = lex_math_span("$x^2$ t", 0) else {
            panic!("inline math scans")
        };
        assert!(!is_block);
        assert_eq!(content_str("$x^2$ t", content), "x^2");
        assert_eq!(resume, 5);

        // The `≥`-rule mirrors backticks: an open of 1 closes at the FIRST `$` of the next `≥1`
        // run, resuming past exactly ONE closing `$` (`$a$$b$` → content `a`, resume 3).
        let MathScan::Math { content, resume, .. } = lex_math_span("$a$$b$", 0) else {
            panic!("scans")
        };
        assert_eq!(content_str("$a$$b$", content), "a");
        assert_eq!(resume, 3);

        // Run-2 inline: a nonempty tail forbids the fence, so `$$a$b$$` is inline run-2 — a single
        // `$` is content, `$$` closes.
        let MathScan::Math { content, is_block, resume, .. } = lex_math_span("$$a$b$$", 0) else {
            panic!("scans")
        };
        assert!(!is_block);
        assert_eq!(content_str("$$a$b$$", content), "a$b");
        assert_eq!(resume, 7);

        // TeX escape: `\$` stays content; the span closes at the real terminator.
        let MathScan::Math { content, .. } = lex_math_span(r"$a \$ b$ t", 0) else {
            panic!("scans")
        };
        assert_eq!(content_str(r"$a \$ b$ t", content), r"a \$ b");

        // No same-line close → literal (the `$`-run is text; resume just past the opener run).
        assert!(matches!(lex_math_span("costs $5 today", 6), MathScan::Literal { resume: 7 }));
        // Inline never crosses a newline.
        assert!(matches!(lex_math_span("$a\nb$", 0), MathScan::Literal { resume: 1 }));

        // Display fence: whitespace-only opener tail, body between the fence lines, resume past the
        // closing `$$`.
        let src = "$$\n\\sum x\n$$\n";
        let MathScan::Math { content, is_block, resume, .. } = lex_math_span(src, 0) else {
            panic!("fence scans")
        };
        assert!(is_block);
        assert_eq!(content_str(src, content), "\\sum x");
        assert_eq!(resume, 12);

        // A nonempty opener tail forbids the fence (math has no info string): `$$x⏎$$` is inline
        // run-2, and with no same-line `≥2` close it is literal.
        assert!(matches!(lex_math_span("$$x\n$$", 0), MathScan::Literal { resume: 2 }));

        // A `<open_len` run inside the fence body does not close it; a `≥ open_len` run does.
        let src = "$$\na $ b\n$$\n";
        let MathScan::Math { content, is_block, .. } = lex_math_span(src, 0) else {
            panic!("scans")
        };
        assert!(is_block);
        assert_eq!(content_str(src, content), "a $ b");
    }

    /// [`armed_boundary`]: a `|@` arms; otherwise the run reaches `bound`.
    #[test]
    fn armed_boundary_arms_and_bounds() {
        // `|@` in range → armed at the `@`, the run ends before the `|`.
        let src = "a b |@x c";
        let (run_end, b) = armed_boundary(src, 0, src.len() as u32);
        assert_eq!(&src[0..run_end as usize], "a b ");
        assert!(matches!(b, ArmedBoundary::ArmedAt { at: 5 }));

        // No `|@` before the bound → Bound, the whole slice is the run.
        let (run_end, b) = armed_boundary("a $ b", 0, 5);
        assert_eq!(run_end, 5);
        assert!(matches!(b, ArmedBoundary::Bound));

        // A `|@` past the bound is not seen (the extent was fixed first).
        let (run_end, b) = armed_boundary("ab|@x", 0, 2);
        assert_eq!(run_end, 2);
        assert!(matches!(b, ArmedBoundary::Bound));
    }

    /// The CommonMark-style line clamp: `*`/`_`/`` ` ``/inline `$` never cross a newline — an
    /// opener with no same-line close is literal. A display `$$` fence and a fenced ``` stay
    /// multi-line.
    #[test]
    fn inline_spans_terminate_at_newline() {
        // Emphasis: a close on a later line is out of scope…
        assert_eq!(find_emphasis_close("*a\nb*", 0, b'*'), None);
        // …but a same-line close still matches, right up to the line end.
        assert_eq!(find_emphasis_close("*a*\nb", 0, b'*'), Some(2));
        // A raw-span skip that crosses the line end kills the span too.
        assert_eq!(find_emphasis_close("*a `x\ny` b*", 0, b'*'), None);
        // An escaped newline cannot extend the scope.
        assert_eq!(find_emphasis_close("*a\\\nb*", 0, b'*'), None);

        // Inline code: the close backtick must sit on the opening line (the motivating case:
        // `- `foo⏎- bar` is two bullets, not one code span).
        assert!(matches!(lex_code_span("`foo\n- bar`", 0), CodeScan::Literal { resume: 1 }));
        let src = "`a` b\n`c`";
        let CodeScan::Code { content, .. } = lex_code_span(src, 0) else {
            panic!("same-line close still scans")
        };
        assert_eq!(&src[content.start as usize..content.end as usize], "a");

        // Inline math: the line end is a boundary → no same-line close → opener literal.
        assert!(matches!(lex_math_span("$a\nb$", 0), MathScan::Literal { resume: 1 }));
        assert!(matches!(lex_math_span("$a\\\nb$", 0), MathScan::Literal { resume: 1 }));
        // A display fence still crosses newlines (a standalone `$$` line opens it).
        let MathScan::Math { is_block, .. } = lex_math_span("$$\na\nb\n$$", 0) else {
            panic!("display fence scans across newlines")
        };
        assert!(is_block);
    }

    /// Attrs-group scans: the first-entry gate, the trailing rule, the `}`-closer allowance, the
    /// balanced/string-aware extent, and the `limit` clip.
    #[test]
    fn attrs_group_scans() {
        let at = |src: &str, closer: bool| attrs_group_at(src, 0, src.len() as u32, closer);

        // The happy shapes: `ident:` first entry, quoted key, spread.
        assert_eq!(at("[id: \"x\"]", false), Some(9));
        assert_eq!(at("[id: \"x\", class: y]  ", false), Some(19)); // trailing ws ok
        assert_eq!(at("[\"data-x\": 1]", false), Some(13));
        assert_eq!(at("[...rest]", false), Some(9));

        // Gate 1 rejects prose-shaped interiors.
        assert!(at("[1]", false).is_none());
        assert!(at("[just words]", false).is_none());
        assert!(at("[x]", false).is_none()); // bare shorthand: no `:` → literal prose
        assert!(at("[x , y]", false).is_none());

        // Gate 2: trailing only — content after the `]` on its line kills it; a depth-0 `}`
        // is allowed only when the caller says the body closes there.
        assert!(at("[k: 1] tail", false).is_none());
        assert!(at("[k: 1]}", false).is_none());
        assert_eq!(at("[k: 1]}", true), Some(6));
        assert_eq!(at("[k: 1] }", true), Some(6));

        // The extent is string-aware and must close within the limit.
        let src = "[k: \"]\"]";
        assert_eq!(at(src, false), Some(src.len() as u32));
        assert!(at("[k: 1", false).is_none()); // unterminated
        assert!(attrs_group_at("[k: 1]", 0, 5, false).is_none()); // clipped by the frame

        // A multi-line group is trailing on its *closing* line.
        assert_eq!(at("[k: 1,\n m: 2]", false), Some(13));
        assert!(at("[k: 1,\n m: 2] t", false).is_none());
    }

    /// Link scans: the `[text](url)` shape, nesting/escapes/skips, the glued `](`, the line
    /// clamp, the `limit` clip, and link-opacity in the emphasis-close scan.
    #[test]
    fn link_scans() {
        let scan = |src: &str, at: u32| lex_link_span(src, at, src.len() as u32);
        let slices = |src: &str, l: &LinkSpans| {
            (
                src[l.text.start as usize..l.text.end as usize].to_string(),
                src[l.url.start as usize..l.url.end as usize].to_string(),
            )
        };

        let src = "[docs](https://x.com/a_b) t";
        let l = scan(src, 0).expect("scans");
        assert_eq!(slices(src, &l), ("docs".into(), "https://x.com/a_b".into()));
        assert_eq!(l.resume, 25);

        // Nested brackets pair; balanced parens in the url pair; escapes hide closers.
        let src = "[a [b] c](u(v)w)";
        let l = scan(src, 0).expect("scans");
        assert_eq!(slices(src, &l), ("a [b] c".into(), "u(v)w".into()));
        let src = r"[a\]b](u\)v)";
        let l = scan(src, 0).expect("scans");
        assert_eq!(slices(src, &l), (r"a\]b".into(), r"u\)v".into()));

        // An `@`-form's groups and a raw span are opaque in the text scan.
        let src = "[see @f[k: \"]\"] and `]`](u)";
        let l = scan(src, 0).expect("scans");
        assert_eq!(slices(src, &l).1, "u");

        // Failures: no glued `(`, no close on the line, a depth-0 `}`, a `//` comment, the limit.
        assert!(scan("[a] (u)", 0).is_none()); // space before `(`
        assert!(scan("[a](u", 0).is_none()); // unclosed url
        assert!(scan("[a\nb](u)", 0).is_none()); // the line clamp
        assert!(scan("[a}](u)", 0).is_none()); // enclosing body closes first
        assert!(scan("[a // b](u)", 0).is_none()); // comment claims the line
        assert!(lex_link_span("[a](u)", 0, 4).is_none()); // clipped by the frame

        // Empty text and empty url are legal shapes.
        let src = "[](u)";
        assert!(scan(src, 0).is_some());
        let src = "[x]()";
        let l = scan(src, 0).expect("scans");
        assert_eq!(slices(src, &l).1, "");

        // Emphasis-close opacity: a `_` inside a link's url cannot close an outer `_` span.
        let src = "_see [x](a_b)_";
        assert_eq!(find_emphasis_close(src, 0, b'_'), Some(src.len() as u32 - 1));
    }

    /// Comment scans: opener shapes, line/block extents, Typst-style nesting, the `limit` clamp,
    /// and the comment-awareness of the emphasis-close and brace-clip scans.
    #[test]
    fn comment_scans() {
        // Opener shapes: `//` and `/*` fire; a lone `/` and an escaped opener do not.
        assert!(comment_can_open("// c", 0));
        assert!(comment_can_open("/* c */", 0));
        assert!(!comment_can_open("/ x", 0));
        assert!(!comment_can_open("a/b", 1));
        assert!(!comment_can_open(r"\// x", 1)); // escaped
        assert!(!comment_can_open("/", 0)); // EOF after the slash

        // Line comment: to the line's content end (`\n` excluded); the limit clamps.
        let src = "a // c\nb";
        let scan = lex_comment(src, 2, src.len() as u32);
        assert!(!scan.block && scan.terminated);
        assert_eq!(&src[2..scan.end as usize], "// c");
        assert_eq!(lex_comment("// abc", 0, 4).end, 4);

        // Block comment: matching `*/`; nesting counts (Typst-style); `/*/` cannot self-close.
        let src = "/* a /* b */ c */ d";
        let scan = lex_comment(src, 0, src.len() as u32);
        assert!(scan.block && scan.terminated);
        assert_eq!(&src[0..scan.end as usize], "/* a /* b */ c */");
        let src = "/*/ */ d";
        assert_eq!(&src[0..lex_comment(src, 0, src.len() as u32).end as usize], "/*/ */");
        let src = "/* a\nb */ c";
        assert_eq!(&src[0..lex_comment(src, 0, src.len() as u32).end as usize], "/* a\nb */");

        // Unterminated → `end == limit`, terminated false.
        let scan = lex_comment("/* a", 0, 4);
        assert!(scan.block && !scan.terminated);
        assert_eq!(scan.end, 4);
    }

    /// Strikethrough `~~` scans: the two-byte opener (word-boundary rule across the pair,
    /// content required, runs literal) and the two-byte close matching.
    #[test]
    fn strike_scans() {
        // Openers.
        assert!(strike_can_open("~~x~~", 0));
        assert!(strike_can_open("a ~~x~~", 2));
        assert!(!strike_can_open("~x", 0)); // no digraph
        assert!(!strike_can_open("~~ x", 0)); // whitespace after the pair
        assert!(!strike_can_open("~~~x", 0)); // a third `~` is not content
        assert!(!strike_can_open("a~~b~~", 1)); // intra-word (wordy on both sides of the pair)
        assert!(!strike_can_open(r"\~~x", 1)); // escaped
        assert!(!strike_can_open("~~", 0)); // EOF after the pair

        // Closes: the first `~~` run preceded by content, two-byte aware.
        assert_eq!(find_strike_close("~~x~~", 0), Some(3));
        assert_eq!(find_strike_close("~~a b~~ c", 0), Some(5));
        assert_eq!(find_strike_close("~~a ~ b~~", 0), Some(7)); // single `~` is content
        assert_eq!(find_strike_close("~~a ~~", 0), None); // whitespace before: cannot close
        assert_eq!(find_strike_close("~~a\nb~~", 0), None); // the line clamp
        assert_eq!(find_strike_close("~~a `x~~y` b~~", 0), Some(12)); // raw span skipped
        assert_eq!(find_strike_close(r"~~a \~~ b~~", 0), Some(9)); // escaped pair cannot close
    }

    #[test]
    fn emphasis_close_honors_comments() {
        // A `//` claims the rest of the line — no close can follow on it.
        assert_eq!(find_emphasis_close("*a // b*", 0, b'*'), None);
        // A `*` inside a block comment is not structure; the close after it matches.
        let src = "*a /* x* */ b*";
        assert_eq!(find_emphasis_close(src, 0, b'*'), Some(src.len() as u32 - 1));
        // A block comment crossing the line end kills the span (the line clamp).
        assert_eq!(find_emphasis_close("*a /* x\ny */ b*", 0, b'*'), None);
    }

    #[test]
    fn brace_clip_honors_comments() {
        // `}` inside a comment is not the clip; the depth-0 `}` after the comment is.
        assert_eq!(brace_clip_on_line("a // }\n", 0), None);
        assert_eq!(brace_clip_on_line("a /* } */ b} t", 0), Some(11));
        // A block comment crossing the line end leaves no depth-0 `}` on this line.
        assert_eq!(brace_clip_on_line("a /* \n */ }", 0), None);
    }

    #[test]
    fn statement_bound_stops_at_percent_or_blank_line() {
        // The next line-leading `%` bounds (bug 5).
        let src = "% a = 1\n% b = 2\nprose\n";
        assert_eq!(statement_bound(src, 1), 8);
        // A blank line bounds (bug 6) — including a whitespace-only line.
        let src = "% a = 1\n\nprose\n";
        assert_eq!(statement_bound(src, 1), 8);
        let src = "% a = 1\n \t\nprose\n";
        assert_eq!(statement_bound(src, 1), 8);
        // Neither → source end (multi-line statements keep flowing under JS grammar).
        let src = "% a = f(\n  1)\nprose\n";
        assert_eq!(statement_bound(src, 1), src.len() as u32);
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

        // A clamped source view (a prefix that ends before the `}|`) caps the scan → Eof: a
        // verbatim armed inside a bounded raw span can't resume past the span's extent.
        let src = "ab}| t";
        let (run_end, b) = verbatim_boundary(&src[..2], 0);
        assert_eq!(run_end, 2);
        assert!(matches!(b, VerbatimBoundary::Eof));
    }

    #[test]
    fn at_line_start_in_frame_predicate() {
        // File offset 0 is a line start (frame_start irrelevant when it is 0).
        assert!(at_line_start_in_frame("@a: b", 0, 0));
        // The frame's own body start counts as a line start, even mid-line: `@{@a: b}` — the
        // inner `@a` sits at the fragment body start (offset 2).
        assert!(at_line_start_in_frame("@{@a: b}", 2, 2));
        // Walking back over spaces/tabs to the frame start still qualifies.
        assert!(at_line_start_in_frame("@p{  @a}", 5, 3));
        // Walking back over spaces/tabs to a newline qualifies (indented line inside a body).
        assert!(at_line_start_in_frame("@p{\n  @a}", 6, 3));
        // Mid-line (a non-whitespace byte precedes, above the frame start) does not.
        assert!(!at_line_start_in_frame("@{x @a}", 4, 2));
        // The walk never crosses below the frame start: a space *before* the frame start does not
        // extend the scan (`@a: @b` — `@b` at 4 is the colon-body start 4, the space at 3 is out).
        assert!(at_line_start_in_frame("@a: @b", 4, 4));
        assert!(!at_line_start_in_frame("@a: @b", 4, 0));
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

        // thematic breaks: a run of 3+ `-`, whitespace-only tail up to the clipped line end
        let t = |src: &str| thematic_break_at(src, 0, line_content_end(src, 0));
        assert_eq!(t("---\n"), Some(Span::new(0, 3)));
        assert_eq!(t("  ----- \nx"), Some(Span::new(2, 7)));
        assert_eq!(t("----"), Some(Span::new(0, 4))); // EOF line
        assert_eq!(t("--\n"), None); // run of 2
        assert_eq!(t("--- x\n"), None); // nonempty tail
        assert_eq!(t("- --\n"), None); // a list line, not a break
        // The clip: `@{---}` hands a line_end at the `}` — the break still fires there.
        assert_eq!(thematic_break_at("---} t", 0, 3), Some(Span::new(0, 3)));
        assert_eq!(thematic_break_at("--- x} t", 0, 6), None);

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

    /// Doc-state sugar scans: opener shapes, the left-boundary guard, the **Typst
    /// minus period** label charset (start `[A-Za-z0-9_]`, continue
    /// `[A-Za-z0-9_:-]`, ASCII-only; digit-start legal, `-`/`:` join, `.`/`$`/Unicode do not),
    /// termination, and the bounded-frame `limit` clip.
    #[test]
    fn docstate_sugar_scans() {
        let lim = |s: &str| s.len() as u32;

        // --- lexer opener shapes (label-start: `[A-Za-z0-9_]`; not `-`/`.`/`$`/space/Unicode) ---
        assert!(label_can_open("<sec>", 0));
        assert!(label_can_open("<_x>", 0));
        assert!(label_can_open("<1a>", 0)); // digit start is legal now (Markdown-style)
        assert!(label_can_open("<2x>", 0)); // digit start
        assert!(!label_can_open("<$x>", 0)); // `$` is NOT a label char now (was a JS ident start)
        assert!(!label_can_open("<λ>", 0)); // Unicode is NOT a label char now
        assert!(!label_can_open("<->", 0)); // `-` is not a start char → `<->` stays literal prose
        assert!(!label_can_open("<-x>", 0)); // `-` start → literal (arrow-like prose)
        assert!(!label_can_open("< b", 0)); // space is not a start char
        assert!(!label_can_open("<", 0)); // EOF
        assert!(!label_can_open(r"\<sec>", 1)); // escaped
        assert!(ref_can_open("&sec", 0));
        assert!(ref_can_open("&1x", 0)); // digit start
        assert!(!ref_can_open("&$x", 0)); // `$` is not a label char now
        assert!(!ref_can_open("&,", 0));
        assert!(!ref_can_open(r"\&x", 1));
        // The footnote opener shape lives in `footnote_sugar_at` itself (the `[^` digraph plus a
        // label-start char; a `[` without them falls through to the link/attrs/literal dispatch).
        // An escaped `\[` never reaches it — the lexer's `[` arm demotes it to text.
        let fn_opens = |src: &str| footnote_sugar_at(src, 0, src.len() as u32).is_some();
        assert!(fn_opens("[^n]"));
        assert!(fn_opens("[^1]")); // digit start legal → `[^1]` fires
        assert!(!fn_opens("[^ x]")); // space after `^`
        assert!(!fn_opens("[^$]")); // `$` is not a label char
        assert!(!fn_opens("[x]")); // no `^`

        // --- left-boundary guard (byte half; frame-start is the parser's) ---
        assert!(docstate_left_guard("<x>", 0)); // start of source
        assert!(docstate_left_guard("a <x>", 2)); // whitespace
        assert!(docstate_left_guard("a\n<x>", 2)); // line start
        for src in ["(<x>", "[<x>", "{<x>", "\"<x>", "'<x>"] {
            assert!(docstate_left_guard(src, 1), "opening punct fires: {src}");
        }
        assert!(!docstate_left_guard("Vec<T>", 3)); // ident before → literal
        assert!(!docstate_left_guard("R&D", 1));
        assert!(!docstate_left_guard("a.<x>", 2)); // closing/other punct → literal
        assert!(!docstate_left_guard("*<x>", 1)); // emphasis marker: only frame-start saves it

        // --- `<label>`: Typst-minus-period charset, `>` required within limit ---
        // `-`/`:` join now (kebab/namespaced labels close on their `>`).
        assert_eq!(label_sugar_at("<sec-intro>", 0, 11), Some(Span::new(1, 10)));
        assert_eq!(&"<sec-intro>"[1..10], "sec-intro");
        let src = "<sec_intro_2> t";
        assert_eq!(label_sugar_at(src, 0, lim(src)), Some(Span::new(1, 12)));
        assert_eq!(&src[1..12], "sec_intro_2");
        // Digit-start label closes on its `>` (`<1a>`).
        assert_eq!(label_sugar_at("<1a> t", 0, 6), Some(Span::new(1, 3)));
        // `.` is NOT a label char: it breaks the label, so `<sec.x>` has no glued `>` → literal.
        assert_eq!(label_sugar_at("<sec.x>", 0, 7), None);
        // A non-ASCII byte breaks the label (`café` → `caf`, then `é` ≠ `>`), so the whole is literal.
        assert_eq!(label_sugar_at("<café> t", 0, 8), None);
        assert_eq!(label_sugar_at("<a b>", 0, 5), None); // space breaks the label
        assert_eq!(label_sugar_at("<ab", 0, 3), None); // no close
        assert_eq!(label_sugar_at("<ab\n>", 0, 5), None); // close not on the opening line
        assert_eq!(label_sugar_at("<ab>", 0, 3), None); // `>` at/past the limit → clipped
        assert_eq!(label_sugar_at("<ab>", 0, 4), Some(Span::new(1, 3)));

        // --- `&ref`: ends at the first non-label char; limit clips the run ---
        assert_eq!(ref_sugar_at("&sec. rest", 0, 10), Some(Span::new(1, 4))); // `.` drops
        assert_eq!(ref_sugar_at("&sec-intro", 0, 10), Some(Span::new(1, 10))); // `-` GLUES (kebab)
        assert_eq!(ref_sugar_at("&sec: x", 0, 7), Some(Span::new(1, 5))); // `:` GLUES (documented)
        assert_eq!(ref_sugar_at("&sec- x", 0, 7), Some(Span::new(1, 5))); // trailing `-` GLUES
        assert_eq!(ref_sugar_at("&1x", 0, 3), Some(Span::new(1, 3))); // digit-start ref
        assert_eq!(ref_sugar_at("&$x", 0, 3), None); // `$` is not a label char now → literal
        assert_eq!(ref_sugar_at("&x", 0, 2), Some(Span::new(1, 2)));
        assert_eq!(ref_sugar_at("&,", 0, 2), None);
        assert_eq!(ref_sugar_at("&abcd", 0, 3), Some(Span::new(1, 3))); // clipped at limit

        // --- `[^mark]`: the digraph + `]` within limit ---
        assert_eq!(footnote_sugar_at("[^note1] t", 0, 10), Some(Span::new(2, 7)));
        assert_eq!(footnote_sugar_at("[^1] t", 0, 6), Some(Span::new(2, 3))); // digit-start mark
        assert_eq!(footnote_sugar_at("[^ x]", 0, 5), None);
        assert_eq!(footnote_sugar_at("[^x y]", 0, 6), None); // label stops at space, `]` missing
        assert_eq!(footnote_sugar_at("[^x]", 0, 3), None); // `]` at/past the limit
        assert_eq!(footnote_sugar_at("[^x]", 0, 4), Some(Span::new(2, 3)));
    }
}
