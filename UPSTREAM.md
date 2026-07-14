# Engine notes: what we work around, and what would delete that code

sheetkit builds on IronCalc pinned at an exact git revision of upstream main
(see the workspace `Cargo.toml`), not the crates.io release. The last
published release (0.7.1, January) predates the dynamic-array engine and
~150 functions (SUMPRODUCT, FILTER, SORT, UNIQUE, SEQUENCE, LET, LAMBDA,
TEXTSPLIT, XMATCH, the financial set, …) that main carries today — a gap
users hit immediately, so we track main and bump the pin deliberately.

The engine is excellent at what it does; these are the gaps we paper over,
tracked here so the workarounds get deleted the moment upstream grows the
capability.

| Gap (at the pinned rev) | Our workaround | Upstream change that deletes it |
|---|---|---|
| No dependents / changed-cells API (the dependency graph is crate-private) | `delta`: snapshot all computed values before a script, diff after. O(non-empty cells) per exec — acceptable because evaluation is full-recalc anyway | Expose a read-only dependents query, or return changed cells from `evaluate()` |
| Full recalc on every edit | `Book::batch` wraps multi-cell commands in `pause_evaluation`/`resume_evaluation` so a script evaluates once per command, not once per cell | Incremental evaluation |
| No sort (the SORT *function* exists; row reorder as an edit does not) | `cmd`: read region contents, reorder rows, re-anchor relative formula references with our own displacer (`displace`) | Engine-level row reorder that displaces formulas (would also fix styles, which we currently do not move). `Model::extend_copied_value` went public at the pinned rev — our displacer could be rebuilt on it |
| No find | `cmd`: walk `sheet_data`, match values/formatted/formulas | Engine-level search is a nice-to-have; ours is fine |
| `paste_csv_string` is tab-delimited (clipboard semantics) | Own RFC-4180 CSV parser in `book` | Rename upstream or accept a delimiter parameter |
| Table (ListObject) CRUD absent (xlsx round-trips tables, no create/edit API) | Region *detection* is ours anyway; Excel table definitions seed region names | Table CRUD + diff variants |
| Constructor lifetimes tie to `&str` params | Create with a static placeholder name, `set_name` after | Take `String`/`impl Into<String>` |
| Merged cells preserved but not editable | Read/flag only | Merge/unmerge API |
| Undo history is in-memory only; diff blobs apply forward only (`Diff` is crate-private) | Exec journal: state N−1 rebuilt by replaying the journal tail onto a base blob; whole-book swaps emit `resync` frames | Serializable history, or a public reverse-apply for diff blobs — would collapse replay-undo into one call and let restore transitions ride the diff queue |

Version pinning is not pedantry: `.ic` snapshots and the diff queue blobs are
bitcode-encoded internal structures, explicitly version-locked. Anything that
replicates workbook state (a browser wasm replica, a checkpoint store) must
run the exact same engine version as the writer. Verified empirically at the
0.7.1 → git-main bump: 0.7.1 blobs fail to decode in the new engine
("invalid packing"), so an engine bump is a breaking protocol change —
migrate workbooks through xlsx, and expect journals/checkpoints to reset.
`ENGINE_VERSION` (served in the channel `welcome` frame) carries the short
git rev so replicas can detect the mismatch instead of corrupting.
