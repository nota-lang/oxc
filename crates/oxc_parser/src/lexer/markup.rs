//! Lexer scan-methods for Nota `@`-markup.
//!
//! Nota is markup-outer, JS-embedded (the inverse of JSX). These scan-methods are the
//! markup analog of [`super::jsx`]'s `next_jsx_child`: the parser drives the lexer into a
//! "markup body" mode to read literal text runs, then re-enters normal JS lexing for embedded
//! `@`-forms and `{`/`}` delimiters.
//!
//! A markup-body text run is returned as a dedicated [`Kind::MarkupText`] token whose value is the
//! *raw source slice* (the parser reads it via `token_source`, never `cur_string`): escape and
//! whitespace processing (the Scribble algorithm, notation.md §Whitespace) is owned by the Nota
//! parser layer (`js/nota.rs`), which keeps embedded spans byte-identical (the §1.6 span-fidelity
//! invariant). The run ends *at* (does not consume) the first markup-significant byte (`}`, `@`, or
//! `{`); those are left for the parser to lex normally, so the parser can track brace depth (a
//! balanced `{…}` inside a body is literal text — Scribble `@foo{f{o}o}` → `"f{o}o"`) and recurse
//! into `@`-forms.

use super::{
    Kind, Lexer, Token,
    search::{SafeByteMatchTable, byte_search, safe_byte_match_table},
};
use crate::config::LexerConfig as Config;

/// Bytes that terminate a Nota markup-text run: `}` (body close), `@` (sigil), `{` (nested body),
/// `\n` (line boundary — so the parser can detect line-start `%`/`%%%` statements, headings, and
/// lists, and apply the Scribble per-line whitespace algorithm), `*`/`_` (the emphasis sigils — the
/// parser applies the Typst word-boundary rule to decide marker-vs-literal), `\` (the general
/// backslash escape — the parser consumes `\<c>` and emits `<c>` literally, the `\` dropped), and the
/// Phase-F raw-span openers `` ` `` (inline/fenced code), `$` (math), and `|` (the `|{ … }|` verbatim
/// body). All are left *unconsumed* for the parser to handle.
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
    /// Scan a run of Nota markup body text, starting at the current source position.
    ///
    /// Advances the source over a maximal run of literal text and returns a [`Token`] spanning it
    /// (kind [`Kind::MarkupText`]). The run stops *before* the first markup-significant byte (`}`,
    /// `@`, `{`) or at EOF; those bytes are left for the parser to lex normally. The token is
    /// produced via `finish_re_lex` so it never pollutes the externally-collected token stream.
    ///
    /// Returns an empty-span `Kind::MarkupText` token if already positioned on a terminator (e.g.
    /// an empty `@tag{}` body), so the parser can detect "no text" without a special EOF dance.
    pub(crate) fn next_markup_text(&mut self) -> Token {
        let start = self.offset();
        self.token.set_start(start);

        // Fast literal-text scan, mirroring `read_jsx_child`'s `byte_search!`. On EOF the run is
        // whatever was consumed up to end-of-file; the parser surfaces the missing `}` as an error.
        byte_search! {
            lexer: self,
            table: MARKUP_TEXT_END_TABLE,
            handle_eof: {
                return self.finish_re_lex(Kind::MarkupText);
            },
        };

        self.finish_re_lex(Kind::MarkupText)
    }
}
