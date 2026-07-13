# Engine notes: what we work around, and what would delete that code

sheetkit builds on IronCalc 0.7.1 (`ironcalc_base` + `ironcalc`, pinned
exactly). The engine is excellent at what it does; these are the gaps we
paper over, tracked here so the workarounds get deleted the moment upstream
grows the capability.

| Gap (0.7.1) | Our workaround | Upstream change that deletes it |
|---|---|---|
| No dependents / changed-cells API (the dependency graph is crate-private) | `delta`: snapshot all computed values before a script, diff after. O(non-empty cells) per exec — acceptable because evaluation is full-recalc anyway | Expose a read-only dependents query, or return changed cells from `evaluate()` |
| Full recalc on every edit | `Book::batch` wraps multi-cell commands in `pause_evaluation`/`resume_evaluation` so a script evaluates once per command, not once per cell | Incremental evaluation |
| No sort | `cmd`: read region contents, reorder rows, re-anchor relative formula references with our own displacer (`displace`) | Engine-level row reorder that displaces formulas (would also fix styles, which we currently do not move) |
| No find | `cmd`: walk `sheet_data`, match values/formatted/formulas | Engine-level search is a nice-to-have; ours is fine |
| `paste_csv_string` is tab-delimited (clipboard semantics) | Own RFC-4180 CSV parser in `book` | Rename upstream or accept a delimiter parameter |
| Table (ListObject) CRUD absent (xlsx round-trips tables, no create/edit API) | Region *detection* is ours anyway; Excel table definitions seed region names | Table CRUD + diff variants |
| Constructor lifetimes tie to `&str` params | Create with a static placeholder name, `set_name` after | Take `String`/`impl Into<String>` |
| Merged cells preserved but not editable | Read/flag only | Merge/unmerge API |

Version pinning is not pedantry: `.ic` snapshots and the diff queue blobs are
bitcode-encoded internal structures, explicitly version-locked. Anything that
replicates workbook state (a browser wasm replica, a checkpoint store) must
run the exact same engine version as the writer.
