---
name: spreadsheets
description: Idioms for working in spreadsheets through the sheets MCP tools (sheet_open/exec/view/save). Use whenever opening, reading, cleaning, computing over, or restructuring xlsx/csv workbooks with these tools.
---

# Working in spreadsheets

You have five tools: `sheet_open`, `sheet_exec`, `sheet_view`, `sheet_save`,
`sheet_close`. The verbs live inside `sheet_exec`'s command language (run
`help` as a script line for the full grammar). The tools are built so you
never need to dump raw cell data — work like this:

## Idioms

1. **Trust the sketch.** `sheet_open` returns every sheet, detected region,
   per-column type/range/fill. Usually you can act immediately — do not start
   by viewing whole sheets.
2. **Address regions, not guesses.** Use region names from the sketch
   (`table1`, or real Excel table names) and header names in `sort … by`.
   They survive row/column insertions; hard-coded A1 bounds do not.
3. **Trust the delta echo.** After every exec you are told exactly which
   cells changed, old ⇒ new, including recalc ripple. Do not re-view ranges
   to confirm what the echo already told you.
4. **Batch lines into one exec.** A script runs top to bottom and stops at
   the first failure, so `set … / fill … / expect …` belongs in ONE call.
5. **`expect` after every non-trivial mutation.** It asserts a cell value and
   fails loudly with the actual. This is your seatbelt; use it liberally —
   especially totals after fills and sorts.
6. **`checkpoint` before structural edits** (sort, insert/delete rows or
   columns, sheet deletion), then `restore <name>` if the result surprises
   you. `undo`/`redo` cover single steps.
7. **Formulas re-anchor on sort and multi-cell set.** Relative references
   follow the row (`=B2*C2` on row 2 stays row-relative); `$` references
   stay put. The output warns when this happened.
8. **Views are budgeted.** Small ranges render dense (formula ⇒ value
   together); large regions render aggregated per-column facts. Elision is
   always announced — if you see none, you saw everything. Use
   `view <target> budget=N` or `mode=dense|agg|sparse` to override.
9. **Highlights are conversation.** `highlight D455 note="breaks the fill"`
   flags a cell for the human; `highlights` lists theirs and yours.

## Example: clean an export and add a computed column

```
sheet_exec:
  checkpoint before-cleanup
  find "n/a" in Qty
  set C150 5
  set E1 Total
  set E2 =C2*D2
  fill E2 -> E2:E201
  expect E2 == 10.5
  sort table1 by Total desc
  expect E2 >= 100
```

One tool call, verified at each step, reversible via the checkpoint.
