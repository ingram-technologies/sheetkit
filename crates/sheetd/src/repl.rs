//! Interactive REPL: the DSL against one workbook, for humans and for
//! developing the encodings. `open`/`save`/`quit` are REPL meta-commands;
//! everything else is the sheetkit command language.

use std::io::{BufRead, Write};

use sheetkit::book::Book;
use sheetkit::cmd;
use sheetkit::session::Session;

pub fn run(path: Option<&str>) -> std::io::Result<()> {
    let mut session = match path {
        Some(p) => match Book::open(p) {
            Ok(b) => {
                println!("opened {p}");
                Session::new(b, Some(p.to_string()))
            }
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        },
        None => Session::new(Book::new_empty("workbook").expect("empty workbook"), None),
    };

    // Show the sketch up front, like sheet_open would.
    {
        let (regions, _) = session.regions().clone();
        println!("{}", sheetkit::view::sketch(&session.book, &regions));
    }
    println!("(sheetkit repl — `help` for commands, `save <path>`, `quit`)");

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    loop {
        print!("> ");
        stdout.flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            break; // EOF
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (word, rest) = match line.find(char::is_whitespace) {
            Some(i) => (&line[..i], line[i..].trim()),
            None => (line, ""),
        };
        match word {
            "quit" | "exit" | ":q" => break,
            "open" => {
                match Book::open(rest) {
                    Ok(b) => {
                        session = Session::new(b, Some(rest.to_string()));
                        let (regions, _) = session.regions().clone();
                        println!("{}", sheetkit::view::sketch(&session.book, &regions));
                    }
                    Err(e) => println!("error: {e}"),
                }
                continue;
            }
            "save" => {
                let (target, overwrite) = match rest.strip_suffix(" overwrite") {
                    Some(p) => (p.trim().to_string(), true),
                    None => (rest.to_string(), false),
                };
                let (target, overwrite) = if target.is_empty() {
                    match &session.origin {
                        Some(o) => (o.clone(), true),
                        None => {
                            println!("error: no origin file; use `save <path>`");
                            continue;
                        }
                    }
                } else {
                    (target, overwrite)
                };
                match session.book.save(&target, overwrite) {
                    Ok(()) => println!("saved to {target}"),
                    Err(e) => println!("error: {e}"),
                }
                continue;
            }
            _ => {}
        }
        let out = cmd::exec(&mut session, line, "user");
        let multi_sheet = session.book.sheet_count() > 1;
        println!("{}", out.render(multi_sheet));
    }
    Ok(())
}
