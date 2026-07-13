//! Acceptance tests for the compressed encodings: big workbooks must sketch
//! inside tight token budgets with every elision announced, never silent.

use sheetkit::book::Book;
use sheetkit::regions::detect_all;
use sheetkit::view::{render_view, sketch, Mode, ViewOptions};

fn approx_tokens(s: &str) -> usize {
    s.chars().count() / 4
}

/// A 50,000-row sales table sketches in well under 3k tokens.
#[test]
fn fifty_k_rows_sketch_under_budget() {
    let mut book = Book::new_empty("big").unwrap();
    let cities = ["Berlin", "Paris", "Madrid", "Rome", "Vienna", "Lisbon"];
    book.batch(|b| {
        b.set_input(0, 1, 1, "Order")?;
        b.set_input(0, 1, 2, "City")?;
        b.set_input(0, 1, 3, "Qty")?;
        b.set_input(0, 1, 4, "UnitPrice")?;
        b.set_input(0, 1, 5, "Total")?;
        for i in 0..50_000i64 {
            let row = (i + 2) as i32;
            b.set_input(0, row, 1, &format!("ORD-{i:06}"))?;
            b.set_input(0, row, 2, cities[(i % 6) as usize])?;
            b.set_input(0, row, 3, &format!("{}", i % 50 + 1))?;
            b.set_input(0, row, 4, &format!("{}.25", i % 400 + 1))?;
            b.set_input(0, row, 5, &format!("=C{row}*D{row}"))?;
        }
        Ok(())
    })
    .unwrap();

    let (regions, _) = detect_all(&book);
    let s = sketch(&book, &regions);

    assert!(
        approx_tokens(&s) <= 3000,
        "sketch is {} tokens:\n{s}",
        approx_tokens(&s)
    );
    // The sketch still carries the load-bearing facts:
    assert!(s.contains("50000 rows + header"), "{s}");
    assert!(s.contains("=C2*D2 fill E2:E50001"), "{s}");
    assert!(s.contains("6 distinct"), "{s}");
    // No silent truncation: a sketch never elides, it aggregates.
    assert!(!s.contains("elided"), "{s}");
}

/// Deviations inside a 50k fill are named, not averaged away.
#[test]
fn deviation_flagged_in_big_fill() {
    let mut book = Book::new_empty("dev").unwrap();
    book.batch(|b| {
        b.set_input(0, 1, 1, "V")?;
        b.set_input(0, 1, 2, "Double")?;
        for row in 2..=20_001 {
            b.set_input(0, row, 1, &(row % 97).to_string())?;
            if row == 4_567 {
                b.set_input(0, row, 2, &format!("=A{row}*3"))?; // the intruder
            } else {
                b.set_input(0, row, 2, &format!("=A{row}*2"))?;
            }
        }
        Ok(())
    })
    .unwrap();
    let (regions, _) = detect_all(&book);
    let s = sketch(&book, &regions);
    assert!(s.contains("breaks fill: B4567"), "{s}");
}

/// A scattered sheet renders as an inverted index within budget.
#[test]
fn sparse_sheet_inverted_index() {
    let mut book = Book::new_empty("scatter").unwrap();
    book.batch(|b| {
        // 300 cells scattered over ~1M positions, only 4 distinct values.
        for i in 0..300i32 {
            let row = (i * 37) % 950 + 1;
            let col = (i * 13) % 40 + 1;
            b.set_input(0, row, col, ["alpha", "beta", "gamma", "42"][(i % 4) as usize])?;
        }
        Ok(())
    })
    .unwrap();
    let (regions, _) = detect_all(&book);
    let t = sheetkit::addr::parse_target("A1:AN950").unwrap();
    let resolved = book.resolve(&t, 0, &Default::default()).unwrap();
    let v = render_view(&book, &resolved, &regions, ViewOptions { mode: Some(Mode::Sparse), budget_tokens: 800 });
    assert!(approx_tokens(&v) <= 800, "{} tokens", approx_tokens(&v));
    assert!(v.contains("non-empty of"), "{v}");
    assert!(v.contains("\"alpha\" —"), "{v}");
    // Either everything fit, or the elision is announced.
    let announced = v.contains("elided for budget");
    let full = v.matches('—').count() == 4;
    assert!(announced || full, "{v}");
}
