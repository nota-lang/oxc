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

use oxc_syntax::identifier::{is_identifier_part, is_identifier_start};

use super::{
    Kind, Lexer, Token,
    search::{SafeByteMatchTable, byte_search, safe_byte_match_table},
};
use crate::{config::LexerConfig as Config, nota::emphasis_can_open};

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
