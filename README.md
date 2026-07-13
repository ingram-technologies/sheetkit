# sheetkit

**An LLM-native spreadsheet toolkit, built on the [IronCalc](https://github.com/ironcalc/IronCalc) engine.**

Spreadsheet interfaces were designed either for humans (grids, selections,
instant visual feedback) or for programs (verbose cell-by-cell APIs). Both
fail language models: a model can't *see* a grid, and dumping 10,000 rows of
JSON into a context window is not vision. sheetkit rebuilds the three things
that make a spreadsheet usable — random-access vision, instant recalc
feedback, and spatial addressing — as **text**:

- **Structure-aware views under a token budget.** Opening a workbook returns a
  *sketch*: every sheet, detected table regions, per-column types, value
  ranges, fill formulas, deviations. A 10,000-row sheet describes itself in a
  few hundred tokens. Small ranges render as a dense grid showing formulas
  *and* computed values together; every truncation is announced, never silent.
- **A delta echo after every mutation.** Change one cell and you're told
  exactly what recalculated: `recalc: 3 cells changed · D2 7 ⇒ 14 · …`.
  The model never needs to re-read a range to know the workbook's state.
- **Range-level verbs.** `fill D2 -> D2:D10001`, `sort orders by Total desc`,
  `expect F9871 == 1204.5` — one line each, not thousands of cell writes.

```text
$ sheetd repl orders.xlsx
workbook "orders" — 1 sheet
Sheet1: used A1:E10001
  table1 (Sheet1!A1:E10001, 10000 rows + header)
    A "Order"      text · sorted asc
    B "City"       text, 6 distinct · values: "Vienna"×1754, "Rome"×1675, …
    C "Date"       date 2024-01-01..2024-12-28
    D "Qty"        number 1..50
    E "UnitPrice"  number 1.09..499.98
> set F1 Total
> set F2 =D2*E2
> fill F2 -> F2:F10001
recalc: 9999 cells changed
  F3 ⇒ 1137.72 · F4 ⇒ 1304.34 · F5 ⇒ 7662.24 · …
> sort table1 by Total desc
⚠ 10000 formula cells moved and re-anchored to their new rows
> expect F2 > 24000
expect F2 > 24000: OK (actual 24495)
```

## MCP server

`sheetd mcp` speaks the [Model Context Protocol](https://modelcontextprotocol.io)
over stdio and exposes five tools: `sheet_open`, `sheet_exec`, `sheet_view`,
`sheet_save`, `sheet_close`. The surface is deliberately small — the command
language carries the verbs, so an agent keeps the whole contract in view.

With [Claude Code](https://claude.com/claude-code):

```sh
cargo build --release
claude mcp add sheets -- /path/to/sheetkit/target/release/sheetd mcp
```

Then ask the agent to open a workbook. A realistic task — "clean this export,
add a margin column, reconcile the totals" — lands in well under ten tool
calls on a 10k-row file, with every intermediate state verifiable from the
transcript alone.

## Server mode

`sheetd serve` runs the same engine as a network service — one authoritative
session per workbook, three doors into it:

```sh
sheetd serve --addr 127.0.0.1:7373 --data-dir ./books --token $SHEETD_TOKEN
```

- **`POST /mcp`** — the same five tools over streamable-HTTP MCP (stateless
  JSON responses; session affinity lives in the `workbook_id` argument).
- **REST workbook API** — `POST /workbooks` (create/import xlsx/csv/ic bytes),
  `GET /workbooks/{id}` (sketch), `POST /workbooks/{id}/exec`,
  `GET /workbooks/{id}/view`, `GET|PUT /workbooks/{id}/file`,
  `GET /workbooks/{id}/highlights`, `DELETE /workbooks/{id}`.
- **`GET /workbooks/{id}/channel`** — a WebSocket where every applied script
  fans out with its recalc delta, the acting principal, exec lifecycle
  (`agent.status`), presence, highlights, and the engine's own diff blob so a
  same-version replica (e.g. the wasm build) can mirror the grid live. See
  [docs/channel-protocol.md](docs/channel-protocol.md).

The blob store under `--data-dir` is the source of truth; resident sessions
are only a cache over it. Every mutation persists the workbook head plus an
**exec journal** (the same `applied` frames the channel broadcasts), which
buys three things at once: `undo` that survives eviction and restarts
(state N−1 = journal base + diff replay), channel replay for reconnecting
clients (`?last_seq=N`), and an audit trail of who changed what. Sequence
numbers are durable.

Because nothing lives only in memory, the server is safe to run multi-tenant:
`--max-resident` caps resident sessions (LRU eviction), `--idle-secs` sweeps
idle ones, `--max-cells` rejects imports too big to evaluate safely, and
`--gc-days` garbage-collects blobs nobody has touched. `DELETE
/workbooks/{id}?purge=true` removes a workbook's files outright.

Auth is a static bearer token; callers are labeled by a configurable
principal header (`SHEETD_PRINCIPAL_HEADER`, default `x-principal`), which is
how a UI knows *who* — human or agent — made each change.

## The command language

One grammar shared by MCP, the REPL, and library callers (`sheet_exec` runs a
script; a failing line stops it and reports the partial state honestly):

```text
sketch · sheets · regions · names · checkpoints · highlights · help
view <target> [mode=dense|agg|sparse] [budget=<tokens>]
set <cell> <value or =formula>          set <range> = [v1, v2, …]
fill <source> -> <target-range>
clear <target> [contents|formats|all]
insert|delete rows|cols … [in <sheet>]
sort <target> by <column or "Header"> [asc|desc][, …]
find "<text>" [in <sheet or range>] [values|formulas]
name <ident> = <range> · unname <ident>
sheet new|select|delete "<name>" · sheet rename "<old>" -> "<new>"
checkpoint <name> · restore <name> · undo · redo
expect <cell> <==|!=|>|>=|<|<=> <value>
highlight <range> [color=<c>] [note="…"] · unhighlight <id>
```

Targets are A1 references (`Sheet2!B3`, `A1:C10`, `C:E`, `5:20`), detected
region names (`table1`, or the Excel table name when defined), defined names,
or sheet names. `sort` understands header names and re-anchors relative
formula references to their new rows. `expect` is the agent's seatbelt:
assert after every non-trivial mutation.

## Architecture

```
crates/sheetkit     the library
  addr     A1/range/target parsing
  book     engine wrapper: I/O (xlsx/csv/ic), batched evaluation, resolution
  delta    snapshot-diff recalc echo
  fills    uniform-fill detection via the engine's shared-formula dedup
  regions  table detection, header inference, per-column stats
  view     dense / aggregated / sparse encodings, the workbook sketch
  cmd      the command language
  session  checkpoints, highlights, workbook registry
crates/sheetd       the binary: MCP over stdio + interactive REPL
```

IronCalc (`ironcalc_base` + `ironcalc`) is pinned exactly: workbook snapshots
(`.ic`) and the engine's diff format are version-locked, and the pin is a
protocol contract for anything that replicates state. See `UPSTREAM.md` for
the engine gaps this crate currently works around and the upstream changes
that would delete that code.

Formulas, by a gift of the engine, are stored deduplicated in canonical R1C1
form — which makes "these 9,999 cells are all `=D{row}*E{row}`" a lookup, not
an analysis, and makes the one cell that *breaks* the pattern stand out. That
single fact powers most of the compression.

## Showcase

The same five tools driven over a real MCP stdio session against a
10,000-row CSV — computed column, verified fill, sort, cross-sheet summary —
with the result opened in the stock IronCalc web app:
[docs/showcase.md](docs/showcase.md).

## Status

Early but real: the five MCP tools (stdio and streamable HTTP), the full
command language, the REST API, the realtime channel, and Google Sheets
pull/push ([docs/gsheets.md](docs/gsheets.md) — two API calls per session:
one `get` at open, one `batchUpdate` diff at save) all work end-to-end —
unit tests plus spawn-the-binary protocol tests for both transports and a
mock-API adapter test, and compression acceptance tests (a 50,000-row
workbook sketches in under 3k tokens with zero silent truncation). Not yet
here: per-workbook access control, multi-replica coordination (the store is
designed for conditional writes but the server currently assumes a single
writer process).

## License

MIT or Apache-2.0, at your option.
