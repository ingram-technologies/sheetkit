//! Displace relative A1 references in a formula by a row delta — copy
//! semantics, used when `sort` moves a formula cell to another row so that
//! row-relative formulas (`=B2*C2` on row 2) keep pointing at their own row.
//!
//! Absolute rows (`B$2`) are left alone, as are references inside string
//! literals. Column letters never change (sort only moves rows).

/// Shift the relative row references of `formula` by `row_delta`.
/// Returns the input unchanged when it is not a formula.
pub fn displace_rows(formula: &str, row_delta: i32) -> String {
    if !formula.starts_with('=') || row_delta == 0 {
        return formula.to_string();
    }
    let chars: Vec<char> = formula.chars().collect();
    let mut out = String::with_capacity(formula.len() + 8);
    let mut i = 0;
    let n = chars.len();
    let mut in_string = false;

    while i < n {
        let c = chars[i];
        if in_string {
            out.push(c);
            if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
                i += 1;
            }
            // Quoted sheet name: copy verbatim through the closing quote.
            '\'' => {
                out.push(c);
                i += 1;
                while i < n {
                    out.push(chars[i]);
                    if chars[i] == '\'' {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            _ if c.is_ascii_alphabetic() || c == '$' => {
                // Try to match a cell reference: [$]?letters[$]?digits with
                // identifier boundaries on both sides.
                let start = i;
                let prev_ok = start == 0 || !is_ident_char(chars[start - 1]);
                let mut j = i;
                let mut col_abs = false;
                if chars[j] == '$' {
                    col_abs = true;
                    j += 1;
                }
                let col_start = j;
                while j < n && chars[j].is_ascii_alphabetic() {
                    j += 1;
                }
                let col_len = j - col_start;
                let mut row_abs = false;
                if j < n && chars[j] == '$' {
                    row_abs = true;
                    j += 1;
                }
                let row_start = j;
                while j < n && chars[j].is_ascii_digit() {
                    j += 1;
                }
                let row_len = j - row_start;
                // `LOG10(` is a function call, not a reference to cell LOG10.
                let next_ok = j >= n || (!is_ident_char(chars[j]) && chars[j] != '(');
                let _ = col_abs;

                if prev_ok && next_ok && (1..=3).contains(&col_len) && (1..=7).contains(&row_len) {
                    let row: i64 = chars[row_start..j]
                        .iter()
                        .collect::<String>()
                        .parse()
                        .unwrap_or(0);
                    let new_row = if row_abs { row } else { row + row_delta as i64 };
                    if (1..=1_048_576).contains(&new_row) {
                        for &ch in &chars[start..row_start] {
                            out.push(ch);
                        }
                        out.push_str(&new_row.to_string());
                        i = j;
                        continue;
                    }
                }
                // Not a reference (function name, defined name…): copy the
                // identifier chunk verbatim so we do not rescan inside it.
                let mut k = start;
                while k < n && is_ident_char(chars[k]) {
                    out.push(chars[k]);
                    k += 1;
                }
                if k == start {
                    out.push(chars[start]);
                    k += 1;
                }
                i = k;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '.'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shifts_relative_rows() {
        assert_eq!(displace_rows("=B2*C2", 3), "=B5*C5");
        assert_eq!(displace_rows("=SUM(A1:A10)", 1), "=SUM(A2:A11)");
    }

    #[test]
    fn keeps_absolute_rows() {
        assert_eq!(displace_rows("=B$2*C2", 3), "=B$2*C5");
        assert_eq!(displace_rows("=$B$2", 5), "=$B$2");
    }

    #[test]
    fn ignores_strings_and_functions() {
        assert_eq!(
            displace_rows("=IF(A1>0,\"A1 ok\",B1)", 1),
            "=IF(A2>0,\"A1 ok\",B2)"
        );
        assert_eq!(displace_rows("=LOG10(A1)", 1), "=LOG10(A2)");
    }

    #[test]
    fn non_formulas_untouched() {
        assert_eq!(displace_rows("hello B2", 3), "hello B2");
        assert_eq!(displace_rows("42", 3), "42");
    }

    #[test]
    fn sheet_qualified() {
        assert_eq!(displace_rows("='My Sheet'!B2+C3", 2), "='My Sheet'!B4+C5");
    }
}
