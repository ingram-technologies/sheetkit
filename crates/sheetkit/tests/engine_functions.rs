//! Pins the engine surface we upgraded for: the git-pinned IronCalc build
//! carries the dynamic-array era (FILTER, UNIQUE, SORT, SEQUENCE, LET,
//! LAMBDA, SUMPRODUCT, …) that the 0.7.1 crates.io release predates. These
//! tests fail loudly if a future pin bump regresses the function set or the
//! spill semantics our delta/view layers rely on.

use sheetkit::book::Book;
use sheetkit::book::Value;
use sheetkit::cmd::exec;
use sheetkit::delta::render;
use sheetkit::session::Session;

fn seeded() -> Book {
    let mut book = Book::new_empty("t").unwrap();
    book.batch(|b| {
        b.set_input(0, 1, 1, "Fruit")?;
        b.set_input(0, 1, 2, "Qty")?;
        for (i, (fruit, qty)) in [("apple", 4), ("pear", 7), ("apple", 2), ("plum", 5)]
            .iter()
            .enumerate()
        {
            let row = i as i32 + 2;
            b.set_input(0, row, 1, fruit)?;
            b.set_input(0, row, 2, &qty.to_string())?;
        }
        Ok(())
    })
    .unwrap();
    book
}

#[test]
fn modern_functions_evaluate() {
    let mut book = seeded();
    book.set_input(0, 1, 4, "=SUMPRODUCT((A2:A5=\"apple\")*B2:B5)")
        .unwrap();
    book.set_input(0, 2, 4, "=LET(x, SUM(B2:B5), x * 2)")
        .unwrap();
    book.set_input(0, 3, 4, "=TEXTJOIN(\",\", TRUE, A2:A3)")
        .unwrap();
    book.evaluate();
    assert_eq!(book.value(0, 1, 4), Value::Number(6.0));
    assert_eq!(book.value(0, 2, 4), Value::Number(36.0));
    assert_eq!(book.value(0, 3, 4), Value::Text("apple,pear".into()));
}

/// A dynamic formula spills; the anchor keeps the formula, spill cells read
/// as plain values (and never as formulas — fill detection depends on that).
#[test]
fn dynamic_arrays_spill() {
    let mut book = seeded();
    book.set_input(0, 1, 6, "=UNIQUE(A2:A5)").unwrap();
    book.evaluate();
    assert_eq!(book.value(0, 1, 6), Value::Text("apple".into()));
    assert_eq!(book.value(0, 2, 6), Value::Text("pear".into()));
    assert_eq!(book.value(0, 3, 6), Value::Text("plum".into()));
    assert!(
        book.formula(0, 1, 6).unwrap().is_some(),
        "anchor keeps its formula"
    );
    assert!(
        book.formula(0, 2, 6).unwrap().is_none(),
        "spill cells are not formulas"
    );
}

/// The delta echo reports every spilled cell, not just the anchor the script
/// touched — the agent learns the whole ripple from one exec.
#[test]
fn spill_ripple_reaches_the_delta() {
    let mut session = Session::new(seeded(), None);
    let out = exec(&mut session, "set F1 =SORT(B2:B5)", "test");
    assert!(out.failed.is_none(), "{:?}", out.failed);
    let delta = render(&out.delta, false, 100);
    for addr in ["F1", "F2", "F3", "F4"] {
        assert!(delta.contains(addr), "missing {addr} in delta:\n{delta}");
    }
    assert!(delta.contains("7"), "sorted max lands in F4:\n{delta}");
}
