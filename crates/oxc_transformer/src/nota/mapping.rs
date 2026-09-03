//! Nota `CodeMapping` marks — Volar structured `CodeMappings`.
//!
//! The Nota lowering ([`super::NotaLowering`]) turns `@`-markup into a plain oxc `Program` emitting
//! Solid JSX. Embedded JS — prop values, `@(expr)`/`@name` interpolation, `%`/`%%%` statement
//! bodies, `|@`-armed forms in raw spans (code / math / verbatim), `@if`/`@for` heads — is spliced
//! as real oxc nodes carrying their *source* spans; component tags (`@Aside` → `<Aside>`) become
//! real identifier references, also source-spanned. Everything the lowering *synthesizes* (the JSX
//! element/fragment/attribute scaffolding, the `<For>`/`<Show>`/`<Dynamic>` wrappers, `String.raw`
//! tagged templates for raw spans) uses `Span::empty`, so it carries no source.
//!
//! This module *exposes* that existing data as Volar `CodeMapping`s. The lowering records a flat
//! list of [`NotaMappingMark`]s — `(source span, kind)` — at each embedded-JS splice / component tag.
//! The compile entry pairs each mark's `span.start` with the **generated** offset codegen emitted the
//! node at (codegen's byte-offset log), and turns the [`NotaMappingKind`] into Volar capability
//! flags. Generated boilerplate is never marked → it is unmapped.
//!
//! Collection is opt-in (`NotaLowering::new(.., collect_mappings)`) so the build/expression paths
//! that do not need mappings stay allocation-free.

use oxc_span::Span;

/// The kind of a Nota source range that maps to generated TS — determines the Volar capability set.
///
/// Embedded-JS ranges get full capabilities; component-identifier ranges get navigation + hover;
/// generated boilerplate is unmapped — i.e. never recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotaMappingKind {
    /// Embedded JavaScript/TypeScript spliced verbatim from the source: a prop value expression, an
    /// `@(expr)`/`@name` interpolation, a `%`/`%%%` statement body, a `|@`-armed form in a raw span
    /// (code / math / verbatim), or an `@if`/`@for` head (condition / iterable / binding). Full IDE capabilities.
    EmbeddedJs,
    /// A component-tag identifier reference: `@Aside` lowering to `<Aside>`. The TS service
    /// resolves it like any identifier (hover, go-to-def, find-references, rename, and the
    /// `@Unknown{}` "Cannot find name" scope error), but it is not a completion/format/structure
    /// region — navigation + hover (semantic) only.
    ComponentIdentifier,
    /// A **props-completion anchor** synthesised by EOF error-recovery for an unclosed `[props]`
    /// group (`@tag[|` at end of file). The mark's `span` is the source `[` (the lowering gives the
    /// JSX opening element that span so codegen logs its position); the join emits a zero-width
    /// segment just inside the opening tag's attribute position with `completion: true`, so the
    /// language server offers JSX prop-name completions there. Not a byte-exact leaf mapping —
    /// resolved specially by the join.
    PropsAnchor,
}

/// One recorded Nota source→generated mapping mark.
///
/// Holds the **source** span of an embedded-JS region or component tag, plus its [`NotaMappingKind`].
/// The generated offset is resolved later (by codegen's offset log) — the lowering only knows source
/// spans.
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

#[cfg(test)]
mod mapping_collection_tests {
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    use super::{NotaMappingKind, NotaMappingMark};
    use crate::NotaLowering;

    /// Parse `src` in document mode, lower it collecting marks, and return them (source-ordered).
    fn marks(src: &str) -> Vec<NotaMappingMark> {
        let allocator = Allocator::default();
        let mut program = Parser::new(&allocator, src, SourceType::nota())
            .parse_nota_document()
            .into_result()
            .unwrap_or_else(|e| panic!("parse failed for {src:?}: {e:?}"));
        NotaLowering::new(&allocator, src, true).lower_document_program(&mut program).mappings
    }

    /// The byte offset of the unique substring `needle` in `src`.
    fn off(src: &str, needle: &str) -> u32 {
        u32::try_from(src.find(needle).expect("needle present")).unwrap()
    }

    fn kind_at(marks: &[NotaMappingMark], start: u32) -> Option<NotaMappingKind> {
        marks.iter().find(|m| m.span.start == start).map(|m| m.kind)
    }

    #[test]
    fn component_tag_is_component_identifier() {
        let src = "@Aside{hi}\n";
        let m = marks(src);
        assert_eq!(kind_at(&m, off(src, "Aside")), Some(NotaMappingKind::ComponentIdentifier));
    }

    #[test]
    fn host_tag_is_not_marked() {
        let src = "@p{hi}\n";
        let m = marks(src);
        // `@p` host tag → no mark (offset 1 is the `p`).
        assert!(kind_at(&m, 1).is_none(), "host tag must not be marked: {m:?}");
    }

    #[test]
    fn interpolation_and_prop_and_statement_are_embedded_js() {
        let src = "% const n: number = x;\n@p[id: theId]{@(user)}\n";
        let m = marks(src);
        // `@(user)` interpolation.
        assert_eq!(kind_at(&m, off(src, "user")), Some(NotaMappingKind::EmbeddedJs));
        // prop value `theId`.
        assert_eq!(kind_at(&m, off(src, "theId")), Some(NotaMappingKind::EmbeddedJs));
        // the `%` statement (its span starts at `const`).
        assert_eq!(kind_at(&m, off(src, "const")), Some(NotaMappingKind::EmbeddedJs));
    }

    #[test]
    fn for_head_binding_and_iterable_are_embedded_js() {
        let src = "@for (item of items) {@item}\n";
        let m = marks(src);
        assert_eq!(kind_at(&m, off(src, "item of")), Some(NotaMappingKind::EmbeddedJs)); // binding
        assert_eq!(kind_at(&m, off(src, "items")), Some(NotaMappingKind::EmbeddedJs)); // iterable
    }

    #[test]
    fn marks_are_source_ordered() {
        let src = "@p[a: x][b: y]{@(z)}\n";
        let m = marks(src);
        let starts: Vec<u32> = m.iter().map(|mk| mk.span.start).collect();
        let mut sorted = starts.clone();
        sorted.sort_unstable();
        assert_eq!(starts, sorted, "marks must be source-ordered: {starts:?}");
    }

    #[test]
    fn collect_flag_off_yields_no_marks() {
        // `collect_mappings = false` (the build / expression path) records nothing — the lowering
        // stays allocation-free. (Stronger than the old test: it inspects the non-collecting result
        // directly, now that both paths return the same `Vec<NotaMappingMark>`.)
        let allocator = Allocator::default();
        let src = "@p[id: theId]{@(user)}\n";
        let mut program = Parser::new(&allocator, src, SourceType::nota())
            .parse_nota_document()
            .into_result()
            .unwrap();
        let marks =
            NotaLowering::new(&allocator, src, false).lower_document_program(&mut program).mappings;
        assert!(marks.is_empty(), "collect=false yields no marks: {marks:?}");
    }
}
