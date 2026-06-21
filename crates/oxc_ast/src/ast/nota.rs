//! Nota markup AST nodes.
//!
//! Nota is a document language whose `@`-syntax markup is parsed into these AST nodes and then
//! *lowered* (in a separate pass) to hyperscript `h`/`Fragment`/`decode` calls — the same
//! parse-then-lower shape oxc uses for JSX (see [`super::jsx`]). The reader lives in
//! `crates/oxc_parser/src/nota/`; the cross-team spec is `design/contract.md` and the
//! implementation memory is `NOTA_READER.md`.
//!
//! NB: `#[ast]`, `#[generate_derive(...)]` and friends are markers consumed by `tasks/ast_tools`;
//! they do not affect the code directly. Run `just ast` after editing this file.
//!
//! **Phase 0 (spike):** a single stub [`NotaMarkup`] node, wired into [`Expression`] at
//! discriminant 40, to prove the AST-tooling integration (generator + estree + raw-transfer). The
//! faithful node set (`NotaDocument`, elements, props, sugar, control flow, verbatim/code/math)
//! lands in Phase 1.

use std::cell::Cell;

use oxc_allocator::{CloneIn, Dummy, TakeIn, UnstableAddress};
use oxc_ast_macros::ast;
use oxc_estree::ESTree;
use oxc_span::{ContentEq, GetSpan, GetSpanMut, Span};
use oxc_syntax::node::NodeId;

use super::js::Expression;

/// A Nota markup form appearing in expression position.
///
/// Stub payload for the Phase-0 spike: it simply wraps an embedded JS [`Expression`], which is
/// enough to exercise recursive visit/traverse descent into the wrapped node. Phase 1 replaces the
/// payload with the faithful `NotaForm` tree (element / fragment / interpolation / control flow /
/// verbatim / code / math) and the document node.
#[ast(visit)]
#[derive(Debug)]
#[generate_derive(CloneIn, Dummy, TakeIn, GetSpan, GetSpanMut, ContentEq, ESTree, UnstableAddress)]
pub struct NotaMarkup<'a> {
    /// Unique identifier for this AST node.
    pub node_id: Cell<NodeId>,
    /// Node location in source code.
    pub span: Span,
    /// The wrapped expression (stub; Phase 1 replaces this with the faithful Nota tree).
    pub expression: Expression<'a>,
}
