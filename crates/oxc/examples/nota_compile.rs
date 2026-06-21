//! Compile a `.nota` file to a JS module string and print it to stdout.
//!
//! The minimal CLI form of `oxc::nota::compile` — used by the cross-stream integration loop
//! (`integration/run.mjs`) and as a demo. The proper CLI is `@nota-lang/cli` (Part 4); the proper
//! library bridge is `@nota-lang/compiler` (Part 3, wasm/napi). This just exposes the entry.
//!
//! ```sh
//! cargo run -q -p oxc --example nota_compile --features codegen -- path/to/doc.nota
//! ```
#![expect(clippy::print_stdout, clippy::print_stderr)]

fn main() {
    let path = std::env::args().nth(1).expect("usage: nota_compile <file.nota>");
    let source = std::fs::read_to_string(&path).expect("read .nota file");

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
