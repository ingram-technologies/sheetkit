//! sheetd — the sheetkit engine server.
//!
//! - `sheetd mcp`            MCP server over stdio (the default)
//! - `sheetd serve`          HTTP: streamable MCP + REST API + realtime channel
//! - `sheetd repl [file]`    interactive DSL session

mod gs;
mod mcp;
mod repl;
mod rpc;
mod serve;
mod tools;

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("mcp") => mcp::serve(),
        Some("repl") => repl::run(args.get(1).map(String::as_str)),
        Some("serve") => {
            let mut opts = serve::ServeOptions {
                addr: "127.0.0.1:7373".to_string(),
                data_dir: None,
                token: std::env::var("SHEETD_TOKEN").ok().filter(|t| !t.is_empty()),
            };
            let mut it = args.iter().skip(1);
            while let Some(arg) = it.next() {
                match arg.as_str() {
                    "--addr" => opts.addr = expect_value(&mut it, "--addr")?,
                    "--data-dir" => opts.data_dir = Some(expect_value(&mut it, "--data-dir")?.into()),
                    "--token" => opts.token = Some(expect_value(&mut it, "--token")?),
                    other => {
                        eprintln!("unknown flag {other:?}\nusage: sheetd serve [--addr HOST:PORT] [--data-dir DIR] [--token TOKEN]");
                        std::process::exit(2);
                    }
                }
            }
            serve::run(opts)
        }
        Some("--version") | Some("-V") => {
            println!("sheetd {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(other) => {
            eprintln!("unknown command {other:?}\nusage: sheetd [mcp | serve [--addr HOST:PORT] [--data-dir DIR] [--token TOKEN] | repl [file]]");
            std::process::exit(2);
        }
    }
}

fn expect_value(it: &mut std::iter::Skip<std::slice::Iter<'_, String>>, flag: &str) -> std::io::Result<String> {
    it.next().cloned().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{flag} needs a value"))
    })
}
