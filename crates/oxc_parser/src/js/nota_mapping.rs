//! Nota CodeMapping marks (H1 — Volar structured `CodeMappings`).
//!
//! The Nota reader lowers `@`-markup to a plain oxc `Program` (D1/D2). Embedded JS — prop values,
//! `@(expr)`/`@name` interpolation, `%`/`%%%` statement bodies, math interpolation, `@if`/`@for`
//! heads — is spliced as real oxc nodes carrying their *source* spans (impl.md §1.6 span fidelity);
//! component tags (`@Aside` → `h(Aside, …)`) become real identifier references, also source-spanned.
//! Everything the reader *synthesizes* (`h(`, `{}`, `[`, `Fragment`, `.map`, `String.raw`, the keyed
//! `Fragment({key:_i},…)` scaffolding) uses `Span::empty`, so it carries no source.
//!
//! H1 *exposes* that existing data as Volar `CodeMapping`s. The reader records a flat list of
//! [`NotaMappingMark`]s — `(source span, kind)` — at each embedded-JS splice / component tag. The
//! `oxc::nota` compile entry pairs each mark's `span.start` with the **generated** offset codegen
//! emitted the node at (codegen's byte-offset log), and turns the [`NotaMappingKind`] into Volar
//! capability flags. Generated boilerplate is never marked → it is unmapped.
//!
//! Collection is opt-in (`ParserImpl::nota_collect_mappings`) so the build/expression entries that
//! do not need mappings stay allocation-free.

use oxc_span::Span;

/// The kind of a Nota source range that maps to generated TS — determines the Volar capability set.
///
/// (impl.md §5.3 / contract §4 H1: embedded-JS ranges get full capabilities; component-identifier
/// ranges get navigation + hover; generated boilerplate is unmapped — i.e. never recorded.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotaMappingKind {
    /// Embedded JavaScript/TypeScript spliced verbatim from the source: a prop value expression, an
    /// `@(expr)`/`@name` interpolation, a `%`/`%%%` statement body, a math `@`-interpolation, or an
    /// `@if`/`@for` head (condition / iterable / binding). Full IDE capabilities.
    EmbeddedJs,
    /// A component-tag identifier reference: `@Aside` lowering to `h(Aside, …)`. The TS service
    /// resolves it like any identifier (hover, go-to-def, find-references, rename, and the
    /// `@Unknown{}` "Cannot find name" scope error), but it is not a completion/format/structure
    /// region — navigation + hover (semantic) only.
    ComponentIdentifier,
}

/// One recorded Nota source→generated mapping mark.
///
/// Holds the **source** span of an embedded-JS region or component tag, plus its [`NotaMappingKind`].
/// The generated offset is resolved later (by codegen's offset log) — the reader only knows source
/// spans at parse time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotaMappingMark {
    /// The source span of the mapped range (byte offsets into the `.nota` source).
    pub span: Span,
    /// What kind of range this is (drives the Volar capability flags downstream).
    pub kind: NotaMappingKind,
}

impl NotaMappingMark {
    /// Construct a mark.
    #[inline]
    pub fn new(span: Span, kind: NotaMappingKind) -> Self {
        Self { span, kind }
    }
}
