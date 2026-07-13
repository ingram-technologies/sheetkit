//! sheetd — the sheetkit engine server.
//!
//! - `sheetd mcp`            MCP server over stdio (the default)
//! - `sheetd repl [file]`    interactive DSL session

mod mcp;
mod repl;
mod tools;

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("mcp") => mcp::serve(),
        Some("repl") => repl::run(args.get(1).map(String::as_str)),
        Some("--version") | Some("-V") => {
            println!("sheetd {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(other) => {
            eprintln!("unknown command {other:?}\nusage: sheetd [mcp | repl [file]]");
            std::process::exit(2);
        }
    }
}
