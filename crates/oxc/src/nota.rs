//! Nota compiler entry — the `nota source → { code, map }` seam.
//!
//! This is the surface that `@nota-lang/compiler` (the wasm/napi wrapper) builds on: the three
//! compile entries. It lives in the `oxc` umbrella crate because that is the only place with *all
//! three* stages on the Nota path available together: the reader (`oxc_parser`, document mode → a
//! faithful Nota AST), the lowering ([`oxc_transformer::NotaLowering`], Nota AST → hyperscript),
//! and `oxc_codegen`. The lowering is the deferred-pass analog of how `oxc_transformer` lowers
//! JSX. (The parse-stage *views* — the playground's `parseAst` document parse and the
//! `parse_nota_highlights` editor spans — are `Parser` entries consumed directly by the wasm
//! bindings; they never reach the lowering, so they don't belong to this compile seam.)
//!
//! The emit is **Solid JSX** (design/solid.md): no imports are emitted here — the structural
//! names, the ambient prelude, and the `solid-js` state surface are all *free names* the
//! `@nota-lang/compiler` wrapper binds. The authoritative name groups are `oxc_transformer`'s
//! `*_EMIT_NAMES` constants, introspectable downstream via the wasm `emitSurface()` entry.

use std::path::{Path, PathBuf};

use oxc_allocator::Allocator;
use oxc_codegen::{Codegen, CodegenOptions, CodegenReturn};
use oxc_diagnostics::OxcDiagnostic;
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use oxc_transformer::{
    JsxOptions, NotaLowering, NotaMappingKind, NotaMappingMark, TransformOptions, Transformer,
    TypeScriptOptions,
};

/// The result of compiling a `.nota` source string.
pub struct NotaCompiled {
    /// The emitted JS module source (document mode: `export default function Doc() { … }`).
    pub code: String,
    /// The source map, if `source_map_path` was provided.
    pub map: Option<oxc_sourcemap::SourceMap>,
    /// The **free names** of the emitted module: identifiers referenced in value position but bound
    /// nowhere in it (root-unresolved references, sorted + deduped). The runtime surface
    /// (`h`/`decode`/`Fragment`/…) always appears — the runtime import is prepended by the wrapper,
    /// not emitted here. The rest is the ambient-prelude surface the lowering references free
    /// (`Tex`, `Heading`, `Label`, …; `secset`-family config calls) plus any genuinely unbound user
    /// references. Mechanism only: *which* of these an integrator binds, and from where, is the
    /// `@nota-lang/compiler` shim's policy (it intersects this list with its ambient-name set to
    /// synthesize the prelude import).
    pub free_names: Vec<String>,
}

// ===================================================================================================
// Volar structured code mappings.
// ===================================================================================================

/// Volar `@volar/language-core` `CodeInformation` capability flags for a mapped range.
///
/// Each boolean enables a class of IDE feature for the range when the TS service result is mapped
/// back to the `.nota` source:
/// * `completion` — autocomplete is offered when the cursor is in this range.
/// * `format`     — the range participates in formatting/document edits.
/// * `navigation` — go-to-definition / find-references / rename cross this range.
/// * `semantic`   — semantic tokens + **hover** are reported for this range.
/// * `structure`  — the range contributes to the document outline / folding.
/// * `verification` — **diagnostics** (type errors, `@Unknown{}` "Cannot find name") surface here.
///
/// Presets: [`MappingCapabilities::full`] (embedded JS/TS) and
/// [`MappingCapabilities::navigation_hover`] (component identifiers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MappingCapabilities {
    /// Autocomplete in this range.
    pub completion: bool,
    /// Formatting / document edits cross this range.
    pub format: bool,
    /// Go-to-definition / find-references / rename cross this range.
    pub navigation: bool,
    /// Semantic tokens + hover for this range.
    pub semantic: bool,
    /// Document outline / folding contribution.
    pub structure: bool,
    /// Diagnostics surface in this range.
    pub verification: bool,
}

impl MappingCapabilities {
    /// Full capabilities — for an **embedded-JS/TS** range (prop value, `@(expr)`/`@name`
    /// interpolation, `%`/`%%%` body, math interpolation, `@if`/`@for` head). Every TS feature
    /// applies to embedded-JS ranges.
    #[must_use]
    pub const fn full() -> Self {
        Self {
            completion: true,
            format: true,
            navigation: true,
            semantic: true,
            structure: true,
            verification: true,
        }
    }

    /// Navigation + hover only — for a **component-identifier** range (`@Aside` → `h(Aside, …)`).
    /// The TS service resolves it like any identifier reference (hover, go-to-def, find-references,
    /// rename, and the `@Unknown{}` "Cannot find name" diagnostic), but it is not a completion-,
    /// formatting-, or structure-region. `verification` stays on so the scope error is reported at
    /// the tag.
    #[must_use]
    pub const fn navigation_hover() -> Self {
        Self {
            completion: false,
            format: false,
            navigation: true,
            semantic: true,
            structure: false,
            verification: true,
        }
    }

    /// Completion only — for the zero-width **props-completion anchor** an EOF-recovered `@tag[|`
    /// leaves just inside the props object literal. It exists purely to route a completion request
    /// into the object type (so the TS service proposes prop names); it must not carry
    /// verification/semantic (there is no real text to diagnose or hover at a zero-width point).
    #[must_use]
    pub const fn props_anchor() -> Self {
        Self {
            completion: true,
            format: false,
            navigation: false,
            semantic: false,
            structure: false,
            verification: false,
        }
    }

    fn from_kind(kind: NotaMappingKind) -> Self {
        match kind {
            NotaMappingKind::EmbeddedJs => Self::full(),
            NotaMappingKind::ComponentIdentifier => Self::navigation_hover(),
            NotaMappingKind::PropsAnchor => Self::props_anchor(),
        }
    }
}

/// One Volar `CodeMapping` — a source⇄generated range correspondence with capability flags.
///
/// Mirrors the `@volar/language-core` `CodeMapping` shape: parallel `source_offsets` /
/// `generated_offsets` / `lengths` arrays (one *segment* each), an optional `generated_lengths` (set
/// only when a segment's generated text differs in length from its source, e.g. codegen normalised
/// `a+b`→`a + b`), and `data` = the capability flags. The Nota reader produces one segment per
/// mapping here (1-element arrays); the language-server `LanguagePlugin` can pass them straight to
/// Volar or coalesce them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeMapping {
    /// Source byte offsets (one per segment).
    pub source_offsets: Vec<u32>,
    /// Generated byte offsets (one per segment), parallel to `source_offsets`.
    pub generated_offsets: Vec<u32>,
    /// Segment lengths in the **source** (one per segment).
    pub lengths: Vec<u32>,
    /// Segment lengths in the **generated** output, when they differ from `lengths`. `None` ⇒ the
    /// generated length equals the source length for every segment (the common case — identifiers
    /// and atoms round-trip byte-for-byte).
    pub generated_lengths: Option<Vec<u32>>,
    /// The capability flags for this mapping's range(s).
    pub data: MappingCapabilities,
}

/// The result of [`compile_with_mappings`] — JS + sourcemap + structured Volar CodeMappings.
pub struct NotaCompiledWithMappings {
    /// The emitted JS module source.
    pub code: String,
    /// The source map, if `source_map_path` was provided.
    pub map: Option<oxc_sourcemap::SourceMap>,
    /// The Volar `CodeMapping`s: source⇄generated ranges with capability flags.
    pub mappings: Vec<CodeMapping>,
}

/// The result of [`compile_virtual`] — the type-preserving virtual `.tsx` emit + code mappings +
/// recovered diagnostics. The virtual path uses EOF error-recovery, so it never fails: an
/// unterminated construct still yields `code` + `mappings`, and the syntax/lowering problems come
/// back in `errors` for the language server to surface as LSP diagnostics.
pub struct NotaVirtualCompiled {
    /// The emitted **virtual TypeScript** (`.tsx`) module source — TS types preserved, for the
    /// language server's TS service.
    pub code: String,
    /// The Volar `CodeMapping`s for the virtual `.tsx`.
    pub mappings: Vec<CodeMapping>,
    /// Recovered Nota parse + lowering diagnostics (byte-spanned). Empty for a well-formed file.
    pub errors: Vec<OxcDiagnostic>,
}

/// Per-call configuration for the one shared Nota compile pipeline ([`compile_internal`]). The three
/// public entries are thin wrappers that differ only in these knobs.
struct CompileConfig {
    /// Strip embedded TypeScript to plain JS (the build path). Mutually exclusive with
    /// `collect_mappings` — stripping shifts codegen offsets, so it never runs on a mapping path.
    strip_ts: bool,
    /// Collect Volar `CodeMapping`s (the mapping / virtual paths) — also enables codegen's offset log.
    collect_mappings: bool,
    /// Tolerate lowering diagnostics (reserved-name collisions) instead of failing: the language
    /// server's virtual `.tsx` path still emits a best-effort file so the editor degrades gracefully
    /// (it surfaces the collision through its own diagnostic channel). The build paths stay strict.
    lenient_diagnostics: bool,
    /// EOF error-recovery: parse with [`Parser::parse_nota_document_recover`] so an unterminated
    /// construct still yields a virtual `.tsx` + mappings, and collect the parse/lowering
    /// diagnostics into [`CompileOutput::errors`] instead of returning `Err`. The language-server
    /// `--virtual` path only; the build paths stay strict (`false`).
    recover: bool,
    /// Source-map path (names the source in the emitted map); `None` skips map generation.
    source_map_path: Option<PathBuf>,
}

/// The output of [`compile_internal`]; each public wrapper takes the fields it exposes.
struct CompileOutput {
    code: String,
    map: Option<oxc_sourcemap::SourceMap>,
    mappings: Vec<CodeMapping>,
    /// Recovered diagnostics (parse + lowering) — non-empty only on the `recover` path.
    errors: Vec<OxcDiagnostic>,
    /// Free (root-unresolved, value-position) names — harvested only on the `strip_ts` build path,
    /// where a semantic pass already runs; empty on the mapping/virtual paths, which don't need it
    /// (the language server prepends its own ambient typing preamble).
    free_names: Vec<String>,
}

/// The one Nota compile pipeline: parse (TS-aware) → Nota-lower → optionally strip TS → codegen,
/// joining mapping marks with the codegen offset log when requested. The public [`compile`],
/// [`compile_with_mappings`], and [`compile_virtual`] are wrappers over this with different
/// [`CompileConfig`]s — keeping the parse mode, the lowering, and the mapping assembly in one place.
///
/// The canonical Nota parse is `SourceType::tsx` (NOTA_READER.md §Compiler entries): embedded
/// TypeScript in `%`/`[props]`/
/// `@(expr)`/`@for` heads is admitted into the AST. The build path then *strips* the types (plain-JS
/// emit); the mapping/virtual paths *preserve* them (the language server's TS service types them).
fn compile_internal(
    source_text: &str,
    config: CompileConfig,
) -> Result<CompileOutput, Vec<OxcDiagnostic>> {
    let allocator = Allocator::default();
    // The recover path (`--virtual`) keeps the partial AST + its diagnostics; the build/mapping
    // paths discard the tree on the first fatal error.
    let (mut program, mut errors) = if config.recover {
        let recovered =
            Parser::new(&allocator, source_text, SourceType::nota()).parse_nota_document_recover();
        (recovered.program, recovered.errors)
    } else {
        let program =
            Parser::new(&allocator, source_text, SourceType::nota()).parse_nota_document()?;
        (program, Vec::new())
    };

    let lowered = NotaLowering::new(&allocator, source_text, config.collect_mappings)
        .lower_document_program(&mut program);
    if !lowered.diagnostics.is_empty() {
        if config.recover {
            // Surface reserved-name-collision diagnostics as editor diagnostics too.
            errors.extend(lowered.diagnostics);
        } else if !config.lenient_diagnostics {
            return Err(lowered.diagnostics);
        }
    }

    let free_names =
        if config.strip_ts { strip_typescript(&allocator, &mut program)? } else { Vec::new() };

    let options = CodegenOptions { source_map_path: config.source_map_path, ..Default::default() };
    let mut codegen = Codegen::new().with_options(options);
    if config.collect_mappings {
        codegen = codegen.with_nota_offset_log();
    }
    let CodegenReturn { code, map, nota_offset_log, .. } = codegen.build(&program);

    let mappings = if config.collect_mappings {
        build_code_mappings(source_text, &code, &lowered.mappings, &nota_offset_log)
    } else {
        Vec::new()
    };
    Ok(CompileOutput { code, map, mappings, errors, free_names })
}

/// Strip embedded TypeScript from the (already Nota-lowered) plain-JS/TS `program` in place, leaving
/// plain JS. Runs `oxc_transformer`'s TypeScript transform only — `EnvOptions::default()` leaves all
/// non-TS JS byte-identical (no arrow/class/etc. lowering), so an all-JS document is unchanged. The
/// transform needs scoping, so a `SemanticBuilder` pass runs first over the lowered program.
///
/// Returns the module's **free names** ([`NotaCompiled::free_names`]), harvested from that same
/// semantic pass: the root-unresolved references that are used in *value* position (a type-only
/// reference — `const n: Foo = …` — is about to be stripped and must not count). Sorted + deduped
/// (the underlying map's iteration order is arbitrary).
fn strip_typescript<'a>(
    allocator: &'a Allocator,
    program: &mut oxc_ast::ast::Program<'a>,
) -> Result<Vec<String>, Vec<OxcDiagnostic>> {
    // The Nota lowering rebuilds the document `Program` with a JSX-flagged `SourceType`; mark it
    // TypeScript too (keeping module-ness) so the embedded TS nodes — already in the AST from the
    // tsx parse — get stripped.
    program.source_type = program.source_type.with_typescript(true);
    let scoping = SemanticBuilder::new().build(program).semantic.into_scoping();
    let mut free_names: Vec<String> = scoping
        .root_unresolved_references()
        .iter()
        .filter(|(_, reference_ids)| {
            reference_ids.iter().any(|&id| scoping.get_reference(id).is_value())
        })
        .map(|(name, _)| (*name).to_string())
        .collect();
    free_names.sort_unstable();
    // TypeScript strip ONLY. `JsxOptions::default()` ENABLES the React JSX transform, which would
    // compile the lowered JSX to `createElement` calls — the emit must stay JSX (the consumer's
    // vite-plugin-solid owns JSX compilation, per target). Explicitly disabled.
    let options = TransformOptions {
        typescript: TypeScriptOptions::default(),
        jsx: JsxOptions::disable(),
        ..Default::default()
    };
    let ret = Transformer::new(allocator, Path::new("doc.nota"), &options)
        .build_with_scoping(scoping, program);
    if ret.errors.is_empty() { Ok(free_names) } else { Err(ret.errors) }
}

/// Compile a `.nota` source string to a JS module (+ optional source map).
///
/// The build path: parses the whole file in Nota *document mode* (markup at the top level → `Doc`),
/// lowers, **strips embedded TypeScript** to plain JS, and runs `oxc_codegen`. On a
/// parse error or a name-collision diagnostic, returns the collected diagnostics (`Err`).
///
/// `source_map_path` controls whether a source map is generated (it names the source in the map);
/// pass `None` to skip map generation (faster).
///
/// # Errors
/// If the source is not well-formed Nota, or a `%` binding collides with a reserved emit name.
pub fn compile(
    source_text: &str,
    source_map_path: Option<PathBuf>,
) -> Result<NotaCompiled, Vec<OxcDiagnostic>> {
    let out = compile_internal(
        source_text,
        CompileConfig {
            strip_ts: true,
            collect_mappings: false,
            lenient_diagnostics: false,
            recover: false,
            source_map_path,
        },
    )?;
    Ok(NotaCompiled { code: out.code, map: out.map, free_names: out.free_names })
}

/// Compile a `.nota` source to JS **plus** structured Volar [`CodeMapping`]s.
///
/// The mapping companion to [`compile`] that exposes the per-range source⇄generated code mappings the
/// language server consumes. Parses TS-aware; codegen **preserves** TS types verbatim (mappings stay
/// byte-exact — it does not strip, unlike the build [`compile`]).
///
/// # Errors
/// If the source is not well-formed Nota, or a `%` binding collides with a reserved emit name.
pub fn compile_with_mappings(
    source_text: &str,
    source_map_path: Option<PathBuf>,
) -> Result<NotaCompiledWithMappings, Vec<OxcDiagnostic>> {
    let out = compile_internal(
        source_text,
        CompileConfig {
            strip_ts: false,
            collect_mappings: true,
            lenient_diagnostics: false,
            recover: false,
            source_map_path,
        },
    )?;
    Ok(NotaCompiledWithMappings { code: out.code, map: out.map, mappings: out.mappings })
}

/// Compile a `.nota` source to the **type-preserving virtual `.tsx`** emit + code mappings.
///
/// The language-server emit: TS-aware parse, and codegen **preserves** the TS type annotations
/// verbatim (no strip step) so the TS service can type the virtual `.tsx`. Returns the virtual code +
/// the [`CodeMapping`]s mapping `.tsx` offsets back to `.nota` offsets.
///
/// **For the Volar `LanguagePlugin`:** like the build path, the runtime `import { h, decode,
/// Fragment, … } from "@nota-lang/runtime"` and the ambient `CodeInline`/`CodeBlock`/`Tex`
/// declarations are *not* emitted here — the plugin prepends that typing preamble to the virtual
/// `.tsx` so `h`/`decode`/component refs type-check. When it does, it must shift every mapping's
/// `generated_offsets` by the prepended prefix length (the `source_offsets` are unchanged — they
/// index the `.nota`).
///
/// Uses **EOF error-recovery**, so it does not fail on unterminated markup: an unclosed `[props]`
/// group, `{ … }` body, or bare `@`-head still yields a virtual `.tsx` (with mappings, incl. a
/// prop-completion anchor at `@tag[|`), and the syntax/lowering problems come back in
/// [`NotaVirtualCompiled::errors`] for the language server to surface as diagnostics.
/// The only `Err` is the internal invariant break in `strip_typescript` — never reached here, since
/// the virtual path does not strip.
///
/// # Errors
/// Practically infallible on the virtual path (recovery + no TS strip); the signature keeps `Result`
/// only to share [`compile_internal`] with the strict build paths.
pub fn compile_virtual(source_text: &str) -> Result<NotaVirtualCompiled, Vec<OxcDiagnostic>> {
    let out = compile_internal(
        source_text,
        CompileConfig {
            strip_ts: false,
            collect_mappings: true,
            lenient_diagnostics: true,
            recover: true,
            source_map_path: None,
        },
    )?;
    Ok(NotaVirtualCompiled { code: out.code, mappings: out.mappings, errors: out.errors })
}

// ===============================================================================================
// `--virtual` JSON serialization — the binary ↔ shim ↔ language-server protocol. Lives here (not
// in the `nota_compile` example) so the contract is testable; the example prints this verbatim.
// The JSON is hand-rolled: no `serde` dependency is added to the published `oxc` crate.
// ===============================================================================================

impl NotaVirtualCompiled {
    /// Serialize as the `nota_compile --virtual` stdout JSON — the contract the
    /// `@nota-lang/compiler` shim's `compileVirtual` and the language server consume:
    ///
    /// ```json
    /// { "code": "<virtual .tsx>",
    ///   "mappings": [ { "sourceOffsets":[u32], "generatedOffsets":[u32], "lengths":[u32],
    ///                   "generatedLengths": [u32]|null,
    ///                   "data": {"completion":bool,"format":bool,"navigation":bool,
    ///                            "semantic":bool,"structure":bool,"verification":bool} } ],
    ///   "errors": [ { "message": string, "start": u32, "len": u32 } ] }
    /// ```
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        out.push_str("{\"code\":");
        push_json_string(&mut out, &self.code);
        out.push_str(",\"mappings\":[");
        for (i, m) in self.mappings.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            write_mapping_json(&mut out, m);
        }
        out.push_str("],\"errors\":[");
        for (i, e) in self.errors.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            write_error_json(&mut out, e);
        }
        out.push_str("]}");
        out
    }
}

/// Serialize one diagnostic as `{ "message": string, "start": u32, "len": u32 }`. The span is the
/// first label's offset/length (byte offsets into the `.nota`); a label-less diagnostic reports
/// `start: 0, len: 0`.
fn write_error_json(out: &mut String, error: &OxcDiagnostic) {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "label offsets/lengths fit u32 (Span model)"
    )]
    let (start, len) = error
        .labels
        .as_ref()
        .and_then(|labels| labels.first())
        .map_or((0u32, 0u32), |label| (label.offset() as u32, label.len() as u32));
    out.push_str("{\"message\":");
    push_json_string(out, &error.message);
    out.push_str(",\"start\":");
    out.push_str(&start.to_string());
    out.push_str(",\"len\":");
    out.push_str(&len.to_string());
    out.push('}');
}

/// Serialize one [`CodeMapping`] as JSON (camelCase keys, parallel u32 arrays).
fn write_mapping_json(out: &mut String, m: &CodeMapping) {
    out.push_str("{\"sourceOffsets\":");
    push_u32_array(out, &m.source_offsets);
    out.push_str(",\"generatedOffsets\":");
    push_u32_array(out, &m.generated_offsets);
    out.push_str(",\"lengths\":");
    push_u32_array(out, &m.lengths);
    out.push_str(",\"generatedLengths\":");
    match &m.generated_lengths {
        Some(v) => push_u32_array(out, v),
        None => out.push_str("null"),
    }
    out.push_str(",\"data\":");
    write_caps_json(out, m.data);
    out.push('}');
}

/// Serialize a [`MappingCapabilities`] as JSON (the six Volar `CodeInformation` flags).
fn write_caps_json(out: &mut String, c: MappingCapabilities) {
    out.push_str("{\"completion\":");
    push_bool(out, c.completion);
    out.push_str(",\"format\":");
    push_bool(out, c.format);
    out.push_str(",\"navigation\":");
    push_bool(out, c.navigation);
    out.push_str(",\"semantic\":");
    push_bool(out, c.semantic);
    out.push_str(",\"structure\":");
    push_bool(out, c.structure);
    out.push_str(",\"verification\":");
    push_bool(out, c.verification);
    out.push('}');
}

fn push_bool(out: &mut String, b: bool) {
    out.push_str(if b { "true" } else { "false" });
}

fn push_u32_array(out: &mut String, xs: &[u32]) {
    out.push('[');
    for (i, x) in xs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        // u32 decimal is always valid JSON number text.
        out.push_str(&x.to_string());
    }
    out.push(']');
}

/// Push a JSON string literal (with surrounding quotes) for `s`, escaping per RFC 8259:
/// `"` `\` `\n` `\r` `\t` `\b` `\f`, and any other control character `< 0x20` as `\u00XX`.
fn push_json_string(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                // Remaining control chars: \u00XX (two lowercase hex digits).
                const HEX: &[u8; 16] = b"0123456789abcdef";
                let code = c as u32;
                out.push_str("\\u00");
                out.push(HEX[((code >> 4) & 0xF) as usize] as char);
                out.push(HEX[(code & 0xF) as usize] as char);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Join the reader's [`NotaMappingMark`]s (source ranges + kinds) with codegen's offset log into
/// Volar [`CodeMapping`]s.
///
/// Codegen logs *every* mapped AST node — both composite nodes (a `VariableDeclaration`, a
/// `CallExpression`) and their leaves (identifiers, literals). Only the **innermost leaves** are
/// byte-exact between source and generated: codegen reformats *inter-token* whitespace (`a+b`→`a +
/// b`, `const   x`→`const x`), which inflates a composite node's generated length, but it never
/// changes an identifier's or number's text — so a leaf maps `[leaf_src, +len) ⇄ [leaf_gen, +len)`
/// exactly (`generated_length == source_length`). We therefore (1) drop any log entry that strictly
/// contains another (the composites), keeping the leaves; (2) for each mark covering `[s, e)`, emit
/// one segment per leaf inside `[s, e)`; and (3) keep only segments whose source slice **equals** the
/// generated slice — this drops the rare non-verbatim leaf (a host-tag `"span"` reinterpreted from
/// the bare source `span`, or a quote-normalised string) that a statement-level mark on a
/// markup-containing component body would otherwise sweep in. The mark's [`NotaMappingKind`] sets the
/// segment's capability flags. A mark matching *no* byte-exact leaf (a body that produced only
/// generated boilerplate) yields no mapping — boilerplate stays unmapped.
fn build_code_mappings(
    source: &str,
    code: &str,
    marks: &[NotaMappingMark],
    offset_log: &[(u32, u32, u32)],
) -> Vec<CodeMapping> {
    // Sort + dedup the log (emit order → range order). Drop zero-length entries. The order is
    // (start ASC, end DESC, gen): a composite node sorts before everything it contains.
    let mut log: Vec<(u32, u32, u32)> =
        offset_log.iter().copied().filter(|&(src_start, src_end, _)| src_end > src_start).collect();
    log.sort_unstable_by_key(|&(start, end, gen_start)| (start, std::cmp::Reverse(end), gen_start));
    log.dedup();

    // Keep only innermost leaves: drop an entry that *strictly* contains another entry's source
    // range (those are composite nodes whose generated text was reformatted, hence not byte-exact).
    // AST spans nest or are disjoint — never partially overlap — so under the sort above an
    // entry's contained entries immediately follow its run of same-span duplicates: entry `k` is
    // a composite iff the next different-span entry starts before `k` ends. One forward pass.
    let leaves: Vec<(u32, u32, u32)> = log
        .iter()
        .enumerate()
        .filter(|&(k, &(start, end, _))| {
            let next_different = log[k + 1..].iter().find(|&&(s2, e2, _)| (s2, e2) != (start, end));
            next_different.is_none_or(|&(s2, _, _)| s2 >= end)
        })
        .map(|(_, &entry)| entry)
        .collect();

    // A leaf segment is valid iff its source slice and generated slice are byte-identical (the
    // verbatim-splice premise). Guards against host-tag string reinterpretation and out-of-range.
    let byte_exact = |src_start: u32, src_end: u32, gen_start: u32| -> bool {
        let (ss, se, gs) = (src_start as usize, src_end as usize, gen_start as usize);
        let len = se - ss;
        se <= source.len()
            && gs + len <= code.len()
            && source.as_bytes()[ss..se] == code.as_bytes()[gs..gs + len]
    };

    let mut mappings = Vec::with_capacity(marks.len());
    for mark in marks {
        // A props-completion anchor (EOF-recovered `@tag[|`) is resolved specially: it is *not* a
        // byte-exact leaf. The lowering gave the JSX opening element the mark's `[` span, so the
        // raw offset log has an entry `(bracket_start, bracket_end, gen_of_open_angle)`. Emit a
        // single **zero-width** segment mapping the source position just after `[` to the
        // generated position just inside the opening tag (after the tag name and its following
        // space, when present) — a completion request there gets the TSX service's JSX
        // *attribute* completions, the JSX-native form of prop completion.
        if mark.kind == NotaMappingKind::PropsAnchor {
            if let Some(&(_, _, gen_angle)) =
                offset_log.iter().find(|&&(gs, ge, _)| gs == mark.span.start && ge == mark.span.end)
            {
                let bytes = code.as_bytes();
                let mut at = gen_angle as usize + 1; // past `<`
                while at < bytes.len()
                    && (bytes[at].is_ascii_alphanumeric()
                        || matches!(bytes[at], b'-' | b'_' | b'$'))
                {
                    at += 1;
                }
                if bytes.get(at) == Some(&b' ') {
                    at += 1;
                }
                #[expect(clippy::cast_possible_truncation)]
                mappings.push(CodeMapping {
                    source_offsets: vec![mark.span.end],
                    generated_offsets: vec![at as u32],
                    lengths: vec![0],
                    generated_lengths: Some(vec![0]),
                    data: MappingCapabilities::props_anchor(),
                });
            }
            continue;
        }

        let (s, e) = (mark.span.start, mark.span.end);
        let data = MappingCapabilities::from_kind(mark.kind);

        // One segment per byte-exact leaf whose source range is contained in this mark's `[s, e)`;
        // leaves are start-sorted, so the candidates are one binary-searched run.
        let lo = leaves.partition_point(|&(src_start, ..)| src_start < s);
        let mut source_offsets = Vec::new();
        let mut generated_offsets = Vec::new();
        let mut lengths = Vec::new();
        for &(src_start, src_end, gen_start) in
            leaves[lo..].iter().take_while(|&&(src_start, ..)| src_start < e)
        {
            if src_end <= e && byte_exact(src_start, src_end, gen_start) {
                source_offsets.push(src_start);
                generated_offsets.push(gen_start);
                lengths.push(src_end - src_start);
            }
        }

        if source_offsets.is_empty() {
            // The marked region produced no byte-exact leaf (only generated boilerplate). Unmapped.
            continue;
        }

        mappings.push(CodeMapping {
            source_offsets,
            generated_offsets,
            lengths,
            generated_lengths: None,
            data,
        });
    }
    mappings
}

#[cfg(test)]
mod tests {
    use super::compile;

    #[test]
    fn compile_basic_document() {
        let out = compile("@h1{Hello}\n", None).expect("compiles");
        assert!(out.code.contains("export default function Doc()"), "{}", out.code);
        assert!(out.code.contains(r#"<h1>{"Hello"}</h1>"#), "{}", out.code);
        assert!(out.code.contains("<NotaDoc>"), "{}", out.code);
        assert!(out.map.is_none());
    }

    #[test]
    fn adjacent_text_coalesces_with_blank_line_marker() {
        // Two paragraphs: the blank line must surface as `\n\n` INSIDE one string child
        // (Reforest's paragraph-break contract), not as separate `"\n"` children.
        let out = compile("one two\n\nthree four\n", None).expect("compiles");
        assert!(
            out.code.contains(r#"{"one two\n\nthree four"}"#),
            "coalesced text with interior blank line:\n{}",
            out.code
        );
    }

    #[test]
    fn list_markers_emit_ulli_and_for_lowers_to_solid_for() {
        let out = compile("@for (x of xs) {\n  - @em{a @(x)}\n}\n", None).expect("compiles");
        assert!(out.code.contains("<For each={xs}>"), "{}", out.code);
        assert!(out.code.contains("<UlLi>"), "{}", out.code);
        assert!(out.code.contains("<em>"), "{}", out.code);
        assert!(!out.code.contains(".map("), "no keyed-map emit remains:\n{}", out.code);
    }

    #[test]
    fn flow_container_interiors_get_reforest() {
        let out = compile("@blockquote{quoted @em{prose}}\n@p{tight}\n", None).expect("compiles");
        assert!(
            out.code.contains("<blockquote><Reforest>"),
            "flow tag wraps its interior:\n{}",
            out.code
        );
        assert!(out.code.contains(r#"<p>{"tight"}</p>"#), "tight tag does not:\n{}", out.code);
    }

    #[test]
    fn compile_emits_source_map_when_requested() {
        let out = compile("@p{hi}\n", Some("doc.nota".into())).expect("compiles");
        assert!(out.map.is_some(), "source map present");
    }

    /// `(line, col)` of byte offset `off` in `text` (0-based; ASCII sources, so byte cols are
    /// fine — the sourcemap's cols are code units).
    fn line_col_of(text: &str, off: usize) -> (u32, u32) {
        let before = &text[..off];
        let line = u32::try_from(before.matches('\n').count()).unwrap();
        let col = u32::try_from(off - before.rfind('\n').map_or(0, |i| i + 1)).unwrap();
        (line, col)
    }

    /// Byte offset of 0-based `(line, col)` in `text`.
    fn offset_at(text: &str, line: u32, col: u32) -> usize {
        let mut off = 0usize;
        for _ in 0..line {
            off += text[off..].find('\n').expect("line in range") + 1;
        }
        off + col as usize
    }

    /// The sourcemap has *content*, not just presence: a known embedded-JS token (`count`)
    /// round-trips — some token's source position is exactly the `.nota` offset of `count`, and
    /// the generated code at that token's generated position is the same text (the byte-exactness
    /// invariant of the mapping tests, applied to the sourcemap channel).
    #[test]
    fn source_map_round_trips_a_known_token() {
        let src = "% const n = count();\n@p{hi}\n";
        let out = compile(src, Some("doc.nota".into())).expect("compiles");
        let map = out.map.expect("source map present");

        assert!(
            map.get_sources().any(|s| s.as_ref() == "doc.nota"),
            "map names the source: {:?}",
            map.get_sources().collect::<Vec<_>>()
        );

        let (src_line, src_col) = line_col_of(src, src.find("count").unwrap());
        let token = map
            .get_tokens()
            .find(|t| t.get_src_line() == src_line && t.get_src_col() == src_col)
            .expect("a token maps the source position of `count`");
        let gen_off = offset_at(&out.code, token.get_dst_line(), token.get_dst_col());
        assert_eq!(
            &out.code[gen_off..gen_off + "count".len()],
            "count",
            "the token's generated position holds the same text:\n{}",
            out.code
        );
    }

    #[test]
    fn compile_reports_errors() {
        match compile("@p{unterminated", None) {
            Ok(_) => panic!("expected a diagnostic for an unterminated body"),
            Err(errors) => assert!(!errors.is_empty()),
        }
    }

    #[test]
    fn compile_accepts_and_strips_embedded_typescript() {
        // The build path parses TS-aware and strips the types → plain, runnable JS.
        let out = compile("% const n: number = 1\n@p{@(n)}\n", None).expect("compiles");
        assert!(
            !out.code.contains(": number"),
            "TS annotation stripped on the build path:\n{}",
            out.code
        );
        assert!(out.code.contains("const n = 1"), "the value survives the strip:\n{}", out.code);
    }

    #[test]
    fn compile_strips_generic_call_type_arguments() {
        // A generic call `f<Foo>(x)` must parse as a call (not `f < Foo > x`), then strip to `f(x)`.
        let out = compile("@p{@(f<Foo>(x))}\n", None).expect("compiles");
        assert!(
            !out.code.contains("f < Foo"),
            "generic not mis-parsed as comparison:\n{}",
            out.code
        );
        assert!(out.code.contains("f(x)"), "type args stripped to a plain call:\n{}", out.code);
    }

    #[test]
    fn compile_reports_reserved_name_collision() {
        // EVERY reserved emit name — `Doc`, the structural components, and the ambient-prelude
        // names the markup lowers to (`Tex`, `Heading`, …) — is a lowering diagnostic when a `%`
        // binding shadows it. Looped over the real list, so extending the emit surface extends
        // this test. (`h` is no longer reserved — the h-call surface is gone.)
        for name in oxc_transformer::reserved_emit_names() {
            match compile(&format!("%let {name} = 1\n@p{{x}}\n"), None) {
                Ok(out) => panic!("`{name}`: expected a collision diagnostic, got:\n{}", out.code),
                Err(errors) => assert!(!errors.is_empty(), "`{name}`: empty diagnostics"),
            }
        }
        assert!(compile("%let h = 1\n@p{x}\n", None).is_ok(), "`h` is an ordinary name now");
        assert!(
            compile("%let mathset = () => 1\n@p{x}\n", None).is_ok(),
            "config fns are ambient but not emit-referenced — not reserved"
        );
    }

    #[test]
    fn free_names_cover_lowering_synthesized_and_user_refs() {
        // `# t` synthesizes a free `Heading` ref; `$x$` a free `Tex`; `% secset(…)` is a free
        // user call; the structural surface (`NotaDoc`) is free because the `@nota-lang/core`
        // import is the wrapper's job. Sorted output.
        let out = compile("% secset({ n: 1 })\n# Title\n\n$y$\n", None).expect("compiles");
        for name in ["Heading", "Tex", "secset", "NotaDoc"] {
            assert!(out.free_names.iter().any(|n| n == name), "{name} free: {:?}", out.free_names);
        }
        for gone in ["h", "decode", "Fragment"] {
            assert!(
                !out.free_names.iter().any(|n| n == gone),
                "{gone} is no longer part of the emit: {:?}",
                out.free_names
            );
        }
        let mut sorted = out.free_names.clone();
        sorted.sort_unstable();
        assert_eq!(out.free_names, sorted, "free names are sorted");
    }

    #[test]
    fn free_names_cover_structural_jsx_references() {
        // List markers → `UlLi`; `@for` → `For`; `@if` → `Show`; a dynamic tag → `Dynamic`. All
        // JSX identifier references, all free (the wrapper binds them).
        let out =
            compile("@for (x of xs) {\n  - @if (x) {@(tags[0]){y}}\n}\n", None).expect("compiles");
        for name in ["NotaDoc", "UlLi", "For", "Show", "Dynamic"] {
            assert!(out.free_names.iter().any(|n| n == name), "{name} free: {:?}", out.free_names);
        }
    }

    #[test]
    fn free_names_exclude_bound_and_textual_mentions() {
        // A `%`-imported name is bound (not free), and prose/string mentions of a name's *text*
        // are not references at all — the regex failure modes the metadata exists to kill.
        // (`Chart`, not `Tex`: emit-surface names are reserved now — importing one is a
        // collision diagnostic, per-doc override happens at the integrator's prelude seam.)
        let out = compile(
            "%import { Chart } from \"./my-chart.js\"\n@p{secset( is not a call}\n$y$\n",
            None,
        )
        .expect("compiles");
        assert!(
            !out.free_names.iter().any(|n| n == "Chart"),
            "imported Chart bound: {:?}",
            out.free_names
        );
        assert!(
            !out.free_names.iter().any(|n| n == "secset"),
            "prose mention: {:?}",
            out.free_names
        );
    }

    #[test]
    fn free_names_exclude_type_only_references() {
        // `Foo` occurs only in a (stripped) type annotation — a type reference, not a value one.
        let out = compile("% const n: Foo = 1\n@p{@(n)}\n", None).expect("compiles");
        assert!(!out.free_names.iter().any(|n| n == "Foo"), "type-only: {:?}", out.free_names);
    }

    #[test]
    fn free_names_respect_nested_shadowing() {
        // `mathset` is bound only inside the component arrow — a name bound at an enclosing scope
        // of its every use is not free; `createSignal` beside it stays free.
        let out = compile(
            "%let C = (props) => { let mathset = () => 1; return createSignal(mathset()); }\n@C{x}\n",
            None,
        )
        .expect("compiles");
        assert!(
            !out.free_names.iter().any(|n| n == "mathset"),
            "locally-bound mathset: {:?}",
            out.free_names
        );
        assert!(
            out.free_names.iter().any(|n| n == "createSignal"),
            "createSignal free: {:?}",
            out.free_names
        );
    }
}

// ===============================================================================================
// Code-mapping + type-preserving virtual emit tests.
// ===============================================================================================
#[cfg(test)]
#[expect(
    clippy::cast_possible_truncation,
    reason = "test fixtures: substring offsets/lengths fit in u32 (oxc's Span model)"
)]
mod h1_h2 {
    use super::{CodeMapping, MappingCapabilities, compile_virtual, compile_with_mappings};

    /// Byte offset of the (unique) substring `needle` in `hay`.
    #[track_caller]
    fn offset_of(hay: &str, needle: &str) -> u32 {
        let i = hay.find(needle).unwrap_or_else(|| panic!("{needle:?} not found in {hay:?}"));
        assert!(!hay[i + needle.len()..].contains(needle), "{needle:?} is not unique in {hay:?}");
        u32::try_from(i).unwrap()
    }

    /// Find the mapping segment whose source offset is `src_off`, returning `(generated_offset,
    /// length, capabilities)`. Asserts exactly one such segment exists.
    #[track_caller]
    fn segment_at(mappings: &[CodeMapping], src_off: u32) -> (u32, u32, MappingCapabilities) {
        let mut found = None;
        for m in mappings {
            for k in 0..m.source_offsets.len() {
                if m.source_offsets[k] == src_off {
                    assert!(
                        found.is_none(),
                        "duplicate mapping segment for source offset {src_off}"
                    );
                    found = Some((m.generated_offsets[k], m.lengths[k], m.data));
                }
            }
        }
        found.unwrap_or_else(|| panic!("no mapping segment for source offset {src_off}"))
    }

    /// Is `src_off` covered by *any* mapping segment?
    fn is_mapped(mappings: &[CodeMapping], src_off: u32) -> bool {
        mappings.iter().any(|m| m.source_offsets.contains(&src_off))
    }

    /// Every mapping segment must round-trip byte-for-byte: the source slice equals the generated
    /// slice (the core invariant — leaves are spliced verbatim).
    #[track_caller]
    fn assert_segments_byte_exact(src: &str, code: &str, mappings: &[CodeMapping]) {
        for m in mappings {
            for k in 0..m.source_offsets.len() {
                let so = m.source_offsets[k] as usize;
                let go = m.generated_offsets[k] as usize;
                let len = m.lengths[k] as usize;
                assert!(so + len <= src.len() && go + len <= code.len());
                assert_eq!(
                    &src[so..so + len],
                    &code[go..go + len],
                    "segment src@{so}+{len} != g@{go}",
                );
            }
        }
    }

    #[test]
    fn embedded_js_in_percent_statement_maps_with_full_caps() {
        // A `%` statement with embedded TS: the identifiers `n` and `count` are byte-exact leaves
        // with full caps; the TS annotation `: number` survives in the emit.
        let src = "% const n: number = count();\n@p{hi}\n";
        let out = compile_with_mappings(src, None).expect("compiles");

        // The TS type annotation is preserved (codegen does not strip — see compile_virtual).
        assert!(out.code.contains(": number"), "type annotation preserved:\n{}", out.code);

        // `count` (a unique identifier) maps to its emitted location with full capabilities.
        let count_src = offset_of(src, "count");
        let (g, len, caps) = segment_at(&out.mappings, count_src);
        assert_eq!(len, 5, "length of `count`");
        assert_eq!(&out.code[g as usize..g as usize + 5], "count", "round-trips to `count`");
        assert_eq!(caps, MappingCapabilities::full(), "embedded JS → full caps");

        assert_segments_byte_exact(src, &out.code, &out.mappings);
    }

    #[test]
    fn prop_expr_and_interpolation_map_with_full_caps() {
        let src = "@p[id: theId]{@(user) world}\n";
        let out = compile_with_mappings(src, None).expect("compiles");

        // Prop value expression `theId` — full caps, byte-exact.
        let (g, len, caps) = segment_at(&out.mappings, offset_of(src, "theId"));
        assert_eq!(len, "theId".len() as u32);
        assert_eq!(&out.code[g as usize..g as usize + len as usize], "theId");
        assert_eq!(caps, MappingCapabilities::full());

        // `@(user)` interpolation — full caps, byte-exact.
        let (g2, _, caps2) = segment_at(&out.mappings, offset_of(src, "user"));
        assert_eq!(&out.code[g2 as usize..g2 as usize + 4], "user");
        assert_eq!(caps2, MappingCapabilities::full());

        assert_segments_byte_exact(src, &out.code, &out.mappings);
    }

    #[test]
    fn component_identifier_maps_with_navigation_hover_only() {
        // `@Aside` → `h(Aside, …)`: navigation + hover, NOT a completion/format/structure region.
        let src = "@Aside{hi}\n";
        let out = compile_with_mappings(src, None).expect("compiles");

        let (g, len, caps) = segment_at(&out.mappings, offset_of(src, "Aside"));
        assert_eq!(len, "Aside".len() as u32);
        assert_eq!(&out.code[g as usize..g as usize + len as usize], "Aside");
        assert_eq!(caps, MappingCapabilities::navigation_hover());
        assert!(caps.navigation && caps.semantic && caps.verification);
        assert!(!caps.completion && !caps.format && !caps.structure);
    }

    #[test]
    fn host_tag_and_generated_boilerplate_are_unmapped() {
        // `@p` is a host tag (emitted as the string `"p"`), NOT a TS symbol → unmapped. The
        // generated `h(`, `{}`, `[`, `decode`, `Fragment` boilerplate is unmapped too.
        let src = "@p{@(x)}\n";
        let out = compile_with_mappings(src, None).expect("compiles");

        // The host tag name `p` in the source is not mapped (its source offset 1).
        let p_src = 1u32; // `@p` → the `p`
        assert_eq!(&src[p_src as usize..=p_src as usize], "p");
        assert!(!is_mapped(&out.mappings, p_src), "host tag `p` must be unmapped");

        // The only mapped source offset is the embedded `x`.
        let x_src = offset_of(src, "x");
        assert!(is_mapped(&out.mappings, x_src), "embedded `x` is mapped");
        let mapped_count: usize = out.mappings.iter().map(|m| m.source_offsets.len()).sum();
        assert_eq!(mapped_count, 1, "only the embedded `x` maps; boilerplate is unmapped");

        assert_segments_byte_exact(src, &out.code, &out.mappings);
    }

    #[test]
    fn virtual_emit_preserves_ts_annotation_and_frames_tsx() {
        // The virtual emit keeps the TS type annotation `: number` (no strip step) and the
        // `@for` head `as` cast, ready for the language server's `.tsx` TS service.
        let src = "% const n: number = count();\n@for (x of xs as string[]) {@x}\n";
        let out = compile_virtual(src).expect("compiles");

        assert!(out.code.contains(": number"), "`: number` preserved:\n{}", out.code);
        assert!(out.code.contains("as string[]"), "`as string[]` preserved:\n{}", out.code);

        // The mappings still resolve embedded identifiers byte-exactly in the virtual `.tsx`.
        let (g, _, caps) = segment_at(&out.mappings, offset_of(src, "count"));
        assert_eq!(&out.code[g as usize..g as usize + 5], "count");
        assert_eq!(caps, MappingCapabilities::full());
    }

    #[test]
    fn round_trip_source_offset_inside_embedded_js() {
        // The headline invariant: a source offset *inside* an embedded-JS span maps to the correct
        // generated offset, and back.
        let src = "@p[onClick: () => go()]{hi}\n";
        let out = compile_with_mappings(src, None).expect("compiles");

        // `go` is inside the embedded prop arrow body; it maps to the `go` in the generated code.
        let go_src = offset_of(src, "go()");
        let (g, _, caps) = segment_at(&out.mappings, go_src);
        assert_eq!(&out.code[g as usize..g as usize + 2], "go", "round-trips to `go`");
        assert_eq!(caps, MappingCapabilities::full());
        assert_eq!(out.code.as_bytes()[g as usize], b'g');
    }

    #[test]
    fn build_and_virtual_share_mappings_modulo_code() {
        // Same parse, two tails: `compile_with_mappings` (build) and `compile_virtual` (.tsx)
        // produce the same mapping structure over the same source ranges.
        let src = "@p[id: theId]{@(user)}\n";
        let build = compile_with_mappings(src, None).expect("compiles");
        let virt = compile_virtual(src).expect("compiles");

        let build_srcs: Vec<u32> =
            build.mappings.iter().flat_map(|m| m.source_offsets.iter().copied()).collect();
        let virt_srcs: Vec<u32> =
            virt.mappings.iter().flat_map(|m| m.source_offsets.iter().copied()).collect();
        assert_eq!(build_srcs, virt_srcs, "same source ranges mapped in both emits");
    }

    /// The canonical golden, exercising the code mappings: the component binding, the `@Colorized`
    /// tag reference, the `@for` iterable + binding, the `@x`/`@(props.children)` interps — all
    /// map byte-exactly, with the right capabilities, and no boilerplate leaks in.
    const CANONICAL_NOTA: &str = "%let Colorized = (props: { children?: unknown }) => {\n  let [color, setColor] = createSignal(\"red\");\n  return @span[onClick: () => setColor(\"green\")][style: {color: color()}]{@(props.children)};\n}\n\n@for (x of items) {\n  - @Colorized{@x}\n}\n";

    #[test]
    fn canonical_golden_mappings_are_byte_exact() {
        let out = compile_with_mappings(CANONICAL_NOTA, None).expect("compiles");

        // Every segment round-trips byte-for-byte (the core invariant).
        assert_segments_byte_exact(CANONICAL_NOTA, &out.code, &out.mappings);

        // The `@Colorized{…}` tag reference (in the `@for` body) maps as a component identifier.
        // (`offset_of` requires uniqueness; `@Colorized` is the 2nd `Colorized` — locate it via the
        // `@` sigil prefix, which is unique.)
        let colorized_tag = (CANONICAL_NOTA.find("@Colorized").unwrap() + 1) as u32;
        let (g, len, caps) = segment_at(&out.mappings, colorized_tag);
        assert_eq!(len, "Colorized".len() as u32);
        assert_eq!(&out.code[g as usize..g as usize + len as usize], "Colorized");
        assert_eq!(caps, MappingCapabilities::navigation_hover(), "tag ref → navigation/hover");

        // The `@for` iterable `items` maps with full caps.
        let (gi, _, capsi) = segment_at(&out.mappings, offset_of(CANONICAL_NOTA, "items"));
        assert_eq!(&out.code[gi as usize..gi as usize + 5], "items");
        assert_eq!(capsi, MappingCapabilities::full());

        // The `%let` binding name `Colorized` (its *declaration*, the 1st occurrence) is part of
        // the `%` statement (a document-local component binding, prepended into Doc) → embedded JS,
        // full caps.
        let colorized_decl = offset_of(CANONICAL_NOTA, "Colorized = ") as usize; // unique form
        let (gd, _, capsd) = segment_at(&out.mappings, colorized_decl as u32);
        assert_eq!(&out.code[gd as usize..gd as usize + "Colorized".len()], "Colorized");
        assert_eq!(capsd, MappingCapabilities::full());
    }
}

// ===============================================================================================
// EOF error-recovery (the `--virtual` recover path): the reader keeps the partial AST + reports
// diagnostics on an unterminated construct, and materialises a prop-completion anchor at `@tag[|`.
// ===============================================================================================
#[cfg(test)]
mod recover {
    use super::compile_virtual;

    /// The load-bearing P5 case: `@a[` at EOF still emits the props object literal, and a mapping
    /// anchors a completion cursor (the position just after `[`) into it (just inside `{`).
    #[test]
    fn unclosed_props_group_yields_opening_tag_with_completion_anchor() {
        let out = compile_virtual("@a[").expect("recovers");

        // The virtual contains the recovered JSX element.
        assert!(out.code.contains("<a"), "recovered opening tag present:\n{}", out.code);

        // A syntax diagnostic is reported, not swallowed.
        assert_eq!(out.errors.len(), 1, "one recovered diagnostic: {:?}", out.errors);

        // The completion anchor: source offset 3 (just after `[`, where the cursor sits) maps to
        // a zero-width generated point *inside the opening tag* (after the tag name), where the
        // TSX service serves JSX attribute completions.
        let anchor = out
            .mappings
            .iter()
            .find(|m| m.source_offsets == vec![3] && m.lengths == vec![0])
            .expect("props-completion anchor mapping present");
        assert!(anchor.data.completion, "anchor carries completion capability");
        let g = anchor.generated_offsets[0] as usize;
        let before = &out.code[..g];
        assert!(
            before.ends_with("<a") || before.ends_with("<a "),
            "anchor is inside the opening tag: {before:?}"
        );
    }

    /// A well-formed file recovers to *exactly* the strict result: no phantom errors, no anchor.
    #[test]
    fn well_formed_input_recovers_identically() {
        let out = compile_virtual("@a[id: x]{ok}\n").expect("compiles");
        assert!(out.errors.is_empty(), "no diagnostics on well-formed input: {:?}", out.errors);
        // No zero-width completion anchor is synthesised (props closed normally).
        assert!(
            !out.mappings.iter().any(|m| m.lengths == vec![0]),
            "no recovery anchor on well-formed input",
        );
    }

    /// An unterminated `{ … }` body keeps its already-collected children and reports the missing
    /// `}` — the body text survives into the virtual for the TS service.
    #[test]
    fn unclosed_body_keeps_children_and_reports() {
        let out = compile_virtual("@p{unterminated").expect("recovers");
        assert!(out.code.contains("\"unterminated\""), "body text preserved:\n{}", out.code);
        assert_eq!(out.errors.len(), 1, "missing-`}}` diagnostic: {:?}", out.errors);
    }

    /// A bare `@` at EOF drops to an empty fragment (no phantom identifier binding) + diagnostic.
    #[test]
    fn bare_at_drops_to_empty_fragment() {
        let out = compile_virtual("@").expect("recovers");
        assert_eq!(out.errors.len(), 1, "bare-`@` diagnostic: {:?}", out.errors);
        // Recovered as `<></>` — no dangling identifier reference.
        assert!(out.code.contains("<></>"), "empty fragment recovery:\n{}", out.code);
    }

    /// Recovery surfaces a reserved-name collision (`%let NotaDoc = …`) as a diagnostic too,
    /// rather than silently dropping it the way the lenient (non-recover) virtual path used to.
    #[test]
    fn reserved_name_collision_surfaces_as_diagnostic() {
        let out = compile_virtual("%let NotaDoc = 1\n@p{x}\n").expect("recovers");
        assert!(!out.errors.is_empty(), "collision surfaced as a diagnostic");
    }

    /// An unterminated verbatim body (`@pre|{` with no `}|`) still yields a framed virtual `.tsx`
    /// (the `Doc` wrapper + the recovered element), with the parse diagnostic surfaced.
    #[test]
    fn unterminated_verbatim_recovers_framed_tsx() {
        let out = compile_virtual("before\n@pre|{\nraw run").expect("recovers");
        assert_eq!(out.errors.len(), 1, "verbatim diagnostic: {:?}", out.errors);
        assert!(out.errors[0].message.contains("verbatim"), "mentions verbatim: {:?}", out.errors);
        assert!(
            out.code.contains("export default function Doc()"),
            "still framed TSX:\n{}",
            out.code
        );
        assert!(out.code.contains("<pre"), "the recovered verbatim element:\n{}", out.code);
    }

    /// The unterminated-`%%%`-fence contract, virtual side: the fence body parses as JS to EOF
    /// with NO diagnostic (`find_fence_close` treats EOF as the close), and the statements land
    /// in the framed emit — the parser-level pin lives in `oxc_parser`'s recover_tests.
    #[test]
    fn unterminated_fence_recovers_silently_with_statements() {
        let out = compile_virtual("%%%\nconst x = 1\n").expect("recovers");
        assert!(
            out.errors.is_empty(),
            "no diagnostic for an EOF-terminated fence: {:?}",
            out.errors
        );
        assert!(
            out.code.contains("export default function Doc()"),
            "still framed TSX:\n{}",
            out.code
        );
        assert!(out.code.contains("const x = 1"), "the fence statement survives:\n{}", out.code);
    }
}

// ===============================================================================================
// The `--virtual` JSON contract ([`NotaVirtualCompiled::to_json`]): the exact key set and shapes
// the `@nota-lang/compiler` shim and the language server parse.
// ===============================================================================================
#[cfg(test)]
mod virtual_json {
    use serde_json::Value;

    use super::compile_virtual;

    fn parse(source: &str) -> Value {
        let out = compile_virtual(source).expect("compiles");
        serde_json::from_str(&out.to_json()).expect("to_json emits valid JSON")
    }

    /// Assert `value` is an object with exactly `keys` (in any order).
    #[track_caller]
    fn assert_keys(value: &Value, keys: &[&str]) {
        let obj = value.as_object().expect("a JSON object");
        let mut got: Vec<&str> = obj.keys().map(String::as_str).collect();
        got.sort_unstable();
        let mut want = keys.to_vec();
        want.sort_unstable();
        assert_eq!(got, want, "exact key set");
    }

    #[test]
    fn top_level_and_mapping_shapes() {
        let json = parse("@a[k: theId]{@(user)}\n");
        assert_keys(&json, &["code", "mappings", "errors"]);

        let code = json["code"].as_str().expect("`code` is a string");
        assert!(code.contains("export default function Doc()"), "framed TSX: {code}");

        let mappings = json["mappings"].as_array().expect("`mappings` is an array");
        assert!(!mappings.is_empty(), "the embedded JS produced mappings");
        for m in mappings {
            assert_keys(
                m,
                &["sourceOffsets", "generatedOffsets", "lengths", "generatedLengths", "data"],
            );
            let source_offsets = m["sourceOffsets"].as_array().expect("array");
            let generated_offsets = m["generatedOffsets"].as_array().expect("array");
            let lengths = m["lengths"].as_array().expect("array");
            assert_eq!(source_offsets.len(), generated_offsets.len(), "parallel arrays");
            assert_eq!(source_offsets.len(), lengths.len(), "parallel arrays");
            for x in source_offsets.iter().chain(generated_offsets).chain(lengths) {
                assert!(x.is_u64(), "offsets/lengths are unsigned numbers: {x:?}");
            }
            // `generatedLengths` is null or a parallel array.
            match &m["generatedLengths"] {
                Value::Null => {}
                Value::Array(v) => assert_eq!(v.len(), source_offsets.len(), "parallel array"),
                other => panic!("generatedLengths must be null or an array: {other:?}"),
            }
            let data = &m["data"];
            assert_keys(
                data,
                &["completion", "format", "navigation", "semantic", "structure", "verification"],
            );
            for flag in data.as_object().unwrap().values() {
                assert!(flag.is_boolean(), "capability flags are booleans: {flag:?}");
            }
        }

        assert_eq!(json["errors"].as_array().expect("array").len(), 0, "well-formed → no errors");
    }

    #[test]
    fn errors_carry_message_and_span() {
        let json = parse("@p{unterminated");
        let errors = json["errors"].as_array().expect("`errors` is an array");
        assert_eq!(errors.len(), 1, "the recovered diagnostic is serialized: {errors:?}");
        for e in errors {
            assert_keys(e, &["message", "start", "len"]);
            assert!(!e["message"].as_str().expect("string").is_empty());
            assert!(e["start"].is_u64() && e["len"].is_u64(), "byte-span numbers: {e:?}");
        }
        // The code is still present and framed on the recover path.
        assert!(json["code"].as_str().unwrap().contains("export default function Doc()"));
    }

    /// The escaping path: `code` contains newlines, quotes, backslashes, and a control char —
    /// round-tripping through a real JSON parser proves the hand-rolled writer escapes correctly.
    #[test]
    fn code_string_escaping_round_trips() {
        let src = "@p{a \"quoted\" \\@ literal}\n";
        let out = compile_virtual(src).expect("compiles");
        let json: Value = serde_json::from_str(&out.to_json()).expect("valid JSON");
        assert_eq!(json["code"].as_str().unwrap(), out.code, "code round-trips exactly");
    }
}
