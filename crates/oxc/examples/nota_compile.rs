//! Compile a `.nota` file to a JS module string and print it to stdout.
//!
#![expect(clippy::exit, reason = "CLI example: exit non-zero on compile error")]
//!
//! The minimal CLI form of `oxc::nota::compile`, for demos and integration testing. The proper CLI
//! is `@nota-lang/cli`; the proper library bridge is `@nota-lang/compiler` (wasm/napi). This just
//! exposes the entry.
//!
//! ```sh
//! cargo run -q -p oxc --example nota_compile --features codegen -- path/to/doc.nota
//! ```
//!
//! ## `--analyze` mode
//!
//! This prints [`oxc::nota::analyze`]'s recoverable editor result as JSON.
//!
//! ```sh
//! cargo run -q -p oxc --example nota_compile --features codegen -- --analyze path/to/doc.nota
//! ```
#![expect(clippy::print_stdout, clippy::print_stderr)]

fn main() {
    let mut args = std::env::args().skip(1);
    let first = args.next().expect("usage: nota_compile [--analyze] <file.nota>");

    if first == "--analyze" {
        let path = args.next().expect("usage: nota_compile --analyze <file.nota>");
        let source = std::fs::read_to_string(&path).expect("read .nota file");
        print!(
            "{}",
            serde_json::to_string(&oxc::nota::analyze(&source)).expect("serialize analysis")
        );
    } else {
        // `first` is the path. Keep the no-flag behavior byte-identical to the original example.
        let source = std::fs::read_to_string(&first).expect("read .nota file");
        match oxc::nota::compile(&source, None) {
            Ok(compiled) => print!("{}", compiled.code),
            Err(errors) => {
                for error in errors {
                    eprintln!("{error:?}");
                }
                std::process::exit(1);
            }
        }
    }
}
