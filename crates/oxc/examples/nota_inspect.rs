//! Inspect *every* stage of the Nota reader pipeline for one input — a fuzzing microscope.
//!
//! Where `nota_compile` prints only the final JS, this dumps the whole pipeline so a divergence from
//! the spec can be localized to the stage that caused it:
//!
//! 1. **INPUT** — echoed with whitespace/control chars made visible (`·`=space, `␉`=tab, `↵`=LF,
//!    `⏎`=CR, `⟪BOM⟫`). Whitespace is semantically load-bearing in Nota (the Scribble pass).
//! 2. **PARSER AST** — the raw `{:#?}` of the post-parse Nota AST (`Program` in document mode, the
//!    `Expression` in `--expr` mode). This is the lexer+parser view; node spans expose lexing bugs.
//! 3. **LOWERED AST** — `--lower` only: the `{:#?}` of the hyperscript AST after `NotaLowering`. By
//!    default this is elided, since CODEGEN JS is its readable form.
//! 4. **CODEGEN JS** — the emitted module (the runtime import the reader omits is *not* prepended;
//!    that is `@nota-lang/compiler`'s job, and the TS front door re-adds it when it evaluates).
//! 5. **JS VALIDITY** — auto: re-parse the emitted JS under the stock oxc parser (the validity
//!    invariant the integration fixtures assert).
//!
//! Each stage runs under [`std::panic::catch_unwind`], so a panic at stage N still reports stages
//! `< N` and labels N. **Build/run under the dev (debug) profile, NOT `--release`:** the workspace
//! sets `panic = "abort"` in release (so unwinding is uncatchable), and debug additionally turns on
//! `debug_assertions` (overflow checks, `debug_assert!`) — both are what we *want* when bug-hunting.
//!
//! ```sh
//! cargo run -q -p oxc --example nota_inspect --features codegen -- --inline '@p{Hello *world*}'
//! cargo run -q -p oxc --example nota_inspect --features codegen -- path/to/doc.nota
//! cargo run -q -p oxc --example nota_inspect --features codegen -- --json --lower -   # stdin → JSON
//! ```
//!
//! The TS front door (`packages/cli/scripts/inspect.ts`) spawns this with `--json`, then evaluates
//! CODEGEN JS through `@nota-lang/runtime` to add the runtime HTML + island-manifest stages.
#![expect(clippy::print_stdout, clippy::print_stderr)]

use std::io::Read;
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Mutex};

use oxc_allocator::Allocator;
use oxc_codegen::Codegen;
use oxc_parser::Parser;
use oxc_span::SourceType;
use oxc_transformer::NotaLowering;

/// Which Nota parse entry to drive.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Whole `.nota` file (`parse_nota_document`). The default; matches real files.
    Document,
    /// A single `@`-form (`parse_nota_expression`). For inspecting one construct in isolation.
    Expression,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Mode::Document => "document",
            Mode::Expression => "expression",
        }
    }
}

/// Everything the pipeline produced for one input. Any field is `None`/empty if its stage did not
/// run (an earlier stage failed or panicked).
#[derive(Default)]
struct Report {
    input: String,
    mode_label: &'static str,
    /// Post-parse AST `{:#?}` (`None` if parse failed or panicked).
    ast: Option<String>,
    /// Post-lowering AST `{:#?}` (only collected when `--lower`).
    lowered: Option<String>,
    /// Emitted JS (`None` if codegen did not run).
    code: Option<String>,
    /// `Some(true/false)` once the validity re-parse ran; `None` if it did not.
    js_valid: Option<bool>,
    /// Stock-oxc re-parse errors (when `js_valid == Some(false)`).
    js_errors: Vec<String>,
    /// Intended parse diagnostics (a well-formed `Err`, distinct from a panic).
    parse_diagnostics: Vec<String>,
    /// `(stage, message)` if some stage panicked. The pipeline stops here.
    panic: Option<(String, String)>,
}

/// Run `f`, returning its value or the captured panic message. Clears then reads `captured`, which
/// the installed panic hook fills with `PanicHookInfo`'s `Display` (message + source location).
fn caught<T>(captured: &Mutex<Option<String>>, f: impl FnOnce() -> T) -> Result<T, String> {
    *captured.lock().unwrap() = None;
    match panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(value) => Ok(value),
        Err(_) => Err(captured
            .lock()
            .unwrap()
            .take()
            .unwrap_or_else(|| "<panic with no message>".to_string())),
    }
}

/// The final, shared stage: re-parse the emitted JS under the **stock** oxc parser. Mirrors the
/// `assert_valid_js` invariant from the integration fixtures.
fn check_validity(captured: &Mutex<Option<String>>, code: &str, report: &mut Report) {
    match caught(captured, || {
        let allocator = Allocator::default();
        let ret = Parser::new(&allocator, code, SourceType::default().with_module(true)).parse();
        let errors: Vec<String> = ret.errors.iter().map(ToString::to_string).collect();
        (!ret.panicked && ret.errors.is_empty(), errors)
    }) {
        Ok((valid, errors)) => {
            report.js_valid = Some(valid);
            report.js_errors = errors;
        }
        Err(message) => report.panic = Some(("validity".to_string(), message)),
    }
}

/// Drive the full pipeline for `source`, isolating each stage's panics.
fn inspect(source: &str, mode: Mode, want_lower: bool) -> Report {
    let mut report =
        Report { input: source.to_string(), mode_label: mode.label(), ..Report::default() };

    // Capture panic messages (with source location) instead of letting the default hook spew to
    // stderr; restore the previous hook when done so the rest of the process behaves normally.
    let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let prev_hook = panic::take_hook();
    {
        let captured = Arc::clone(&captured);
        panic::set_hook(Box::new(move |info| {
            *captured.lock().unwrap() = Some(info.to_string());
        }));
    }

    // One arena for the whole reader pipeline; it must outlive every borrowed AST below. Both modes
    // parse as plain mjs (matching `oxc::nota::compile`); embedded TS is out of scope for the reader.
    let allocator = Allocator::default();
    let source_type = SourceType::default();

    match mode {
        Mode::Document => {
            run_document(&allocator, source, source_type, want_lower, &captured, &mut report);
        }
        Mode::Expression => {
            run_expression(&allocator, source, source_type, want_lower, &captured, &mut report);
        }
    }

    panic::set_hook(prev_hook);
    report
}

/// Document-mode pipeline: `parse_nota_document` → `lower_document_program` → `Codegen::build`.
fn run_document<'a>(
    allocator: &'a Allocator,
    source: &'a str,
    source_type: SourceType,
    want_lower: bool,
    captured: &Mutex<Option<String>>,
    report: &mut Report,
) {
    // Stage 1 — parse.
    let parsed =
        caught(captured, || Parser::new(allocator, source, source_type).parse_nota_document());
    let mut program = match parsed {
        Err(message) => {
            report.panic = Some(("parse".to_string(), message));
            return;
        }
        Ok(Err(diagnostics)) => {
            report.parse_diagnostics = diagnostics.iter().map(ToString::to_string).collect();
            return;
        }
        Ok(Ok(program)) => program,
    };
    report.ast = Some(format!("{program:#?}"));

    // Stage 2 — lower (mutates the program in place; move it in, hand it back out).
    let lowered = caught(captured, move || {
        NotaLowering::new(allocator, source, false).lower_document_program(&mut program);
        program
    });
    let program = match lowered {
        Err(message) => {
            report.panic = Some(("lower".to_string(), message));
            return;
        }
        Ok(program) => program,
    };
    if want_lower {
        report.lowered = Some(format!("{program:#?}"));
    }

    // Stage 3 — codegen.
    match caught(captured, || Codegen::new().build(&program).code) {
        Err(message) => {
            report.panic = Some(("codegen".to_string(), message));
            return;
        }
        Ok(code) => report.code = Some(code),
    }

    // Stage 4 — validity.
    if let Some(code) = report.code.clone() {
        check_validity(captured, &code, report);
    }
}

/// Expression-mode pipeline: `parse_nota_expression` → `lower_expression` → `print_expression`.
fn run_expression<'a>(
    allocator: &'a Allocator,
    source: &'a str,
    source_type: SourceType,
    want_lower: bool,
    captured: &Mutex<Option<String>>,
    report: &mut Report,
) {
    // Stage 1 — parse.
    let parsed =
        caught(captured, || Parser::new(allocator, source, source_type).parse_nota_expression());
    let mut expr = match parsed {
        Err(message) => {
            report.panic = Some(("parse".to_string(), message));
            return;
        }
        Ok(Err(diagnostics)) => {
            report.parse_diagnostics = diagnostics.iter().map(ToString::to_string).collect();
            return;
        }
        Ok(Ok(expr)) => expr,
    };
    report.ast = Some(format!("{expr:#?}"));

    // Stage 2 — lower.
    let lowered = caught(captured, move || {
        NotaLowering::new(allocator, source, false).lower_expression(&mut expr);
        expr
    });
    let expr = match lowered {
        Err(message) => {
            report.panic = Some(("lower".to_string(), message));
            return;
        }
        Ok(expr) => expr,
    };
    if want_lower {
        report.lowered = Some(format!("{expr:#?}"));
    }

    // Stage 3 — codegen.
    match caught(captured, || {
        let mut codegen = Codegen::new();
        codegen.print_expression(&expr);
        codegen.into_source_text()
    }) {
        Err(message) => {
            report.panic = Some(("codegen".to_string(), message));
            return;
        }
        Ok(code) => report.code = Some(code),
    }

    // Stage 4 — validity.
    if let Some(code) = report.code.clone() {
        check_validity(captured, &code, report);
    }
}

// ===================================================================================================
// Input echo: make whitespace + control characters visible.
// ===================================================================================================

/// Render `source` with whitespace/control characters shown as glyphs (for the INPUT echo only —
/// every other stage prints raw). A leading UTF-8 BOM becomes `⟪BOM⟫`; newlines keep a real `\n` so
/// the echo still wraps.
fn ws_visible(source: &str) -> String {
    let mut out = String::new();
    let body = match source.strip_prefix('\u{feff}') {
        Some(rest) => {
            out.push_str("⟪BOM⟫");
            rest
        }
        None => source,
    };
    for ch in body.chars() {
        match ch {
            ' ' => out.push('·'),
            '\t' => out.push('␉'),
            '\r' => out.push('⏎'),
            '\n' => {
                out.push('↵');
                out.push('\n');
            }
            c if (c as u32) < 0x20 => {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                let code = c as u32;
                out.push('⟨');
                out.push(HEX[((code >> 4) & 0xF) as usize] as char);
                out.push(HEX[(code & 0xF) as usize] as char);
                out.push('⟩');
            }
            c => out.push(c),
        }
    }
    out
}

// ===================================================================================================
// Text output.
// ===================================================================================================

/// Print a section header rule, e.g. `──────────── PARSER AST`.
fn section(title: &str) {
    println!("\n{:─<12} {title}", "");
}

/// Print the human-readable, sectioned report.
fn print_text(report: &Report, want_ast: bool, want_lower: bool) {
    section(&format!("INPUT (mode: {})", report.mode_label));
    println!("{}", ws_visible(&report.input));

    if want_ast && let Some(ast) = &report.ast {
        section("PARSER AST");
        println!("{ast}");
    }

    if !report.parse_diagnostics.is_empty() {
        section("PARSE DIAGNOSTICS");
        for diagnostic in &report.parse_diagnostics {
            println!("  • {diagnostic}");
        }
        println!("\n(parse returned diagnostics — later stages skipped)");
    }

    if want_lower && let Some(lowered) = &report.lowered {
        section("LOWERED AST");
        println!("{lowered}");
    }

    if let Some(code) = &report.code {
        section("CODEGEN JS");
        println!("{code}");
    }

    match report.js_valid {
        Some(true) => {
            section("JS VALIDITY");
            println!("✓ re-parses cleanly under stock oxc");
        }
        Some(false) => {
            section("JS VALIDITY");
            println!("✗ emitted JS did NOT re-parse under stock oxc:");
            for error in &report.js_errors {
                println!("  • {error}");
            }
        }
        None => {}
    }

    if let Some((stage, message)) = &report.panic {
        section("‼ PANIC");
        println!("stage `{stage}` panicked:\n{message}\n\n(subsequent stages skipped)");
    }
}

// ===================================================================================================
// JSON output (hand-rolled, no serde — matching `nota_compile`).
// ===================================================================================================

/// Print the report as a single JSON object (consumed by `inspect.ts`).
fn print_json(report: &Report) {
    let mut out = String::from("{\"mode\":");
    push_json_string(&mut out, report.mode_label);

    out.push_str(",\"input\":");
    push_json_string(&mut out, &report.input);

    out.push_str(",\"ast\":");
    push_opt_json_string(&mut out, report.ast.as_deref());

    out.push_str(",\"lowered\":");
    push_opt_json_string(&mut out, report.lowered.as_deref());

    out.push_str(",\"code\":");
    push_opt_json_string(&mut out, report.code.as_deref());

    out.push_str(",\"jsValid\":");
    match report.js_valid {
        Some(true) => out.push_str("true"),
        Some(false) => out.push_str("false"),
        None => out.push_str("null"),
    }

    out.push_str(",\"jsErrors\":");
    push_json_string_array(&mut out, &report.js_errors);

    out.push_str(",\"parseDiagnostics\":");
    push_json_string_array(&mut out, &report.parse_diagnostics);

    out.push_str(",\"panic\":");
    match &report.panic {
        Some((stage, message)) => {
            out.push_str("{\"stage\":");
            push_json_string(&mut out, stage);
            out.push_str(",\"message\":");
            push_json_string(&mut out, message);
            out.push('}');
        }
        None => out.push_str("null"),
    }

    out.push('}');
    print!("{out}");
}

/// `null` or a JSON string.
fn push_opt_json_string(out: &mut String, value: Option<&str>) {
    match value {
        Some(s) => push_json_string(out, s),
        None => out.push_str("null"),
    }
}

/// A JSON array of strings.
fn push_json_string_array(out: &mut String, items: &[String]) {
    out.push('[');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        push_json_string(out, item);
    }
    out.push(']');
}

/// Push a JSON string literal (with surrounding quotes), escaping per RFC 8259. Copied from
/// `nota_compile.rs` (the published `oxc` crate avoids a `serde` dependency).
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

// ===================================================================================================
// CLI.
// ===================================================================================================

fn read_stdin() -> String {
    let mut buffer = String::new();
    std::io::stdin().read_to_string(&mut buffer).expect("read stdin");
    buffer
}

const USAGE: &str = "usage: nota_inspect [--json] [--lower] [--expr] [--no-ast] \
                     (--inline <src> | <file.nota> | -)";

fn main() {
    let mut json = false;
    let mut want_lower = false;
    let mut expr = false;
    let mut no_ast = false;
    let mut inline: Option<String> = None;
    let mut path: Option<String> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--lower" => want_lower = true,
            "--expr" => expr = true,
            "--no-ast" => no_ast = true,
            "--inline" => inline = Some(args.next().expect("--inline needs a value")),
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            _ => path = Some(arg),
        }
    }

    let source = match (inline, path) {
        (Some(src), _) => src,
        (None, Some(p)) if p == "-" => read_stdin(),
        (None, Some(p)) => std::fs::read_to_string(&p).expect("read .nota file"),
        (None, None) => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };

    let mode = if expr { Mode::Expression } else { Mode::Document };
    let report = inspect(&source, mode, want_lower || json);

    if json {
        print_json(&report);
    } else {
        print_text(&report, !no_ast, want_lower);
    }
}
