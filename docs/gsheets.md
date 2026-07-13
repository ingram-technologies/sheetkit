# Google Sheets pull/push

The Google Sheets adapter is an *edge adapter*: the remote API is touched
exactly twice per session — one `spreadsheets.get` at open, one `batchUpdate`
at save. Everything in between runs against the local engine, so Google's
latency, quotas, and range limits never enter the working loop.

## Usage

```text
export GSHEETS_TOKEN=<OAuth2 access token with a spreadsheets scope>

sheet_open  { path: "https://docs.google.com/spreadsheets/d/<id>/edit" }
            # or "gsheets:<id>". Pulls once; returns the usual sketch.
sheet_exec  …                # everything is local from here
sheet_save  { }              # pushes the content diff back in one batchUpdate
```

Configuration is environment-only, deliberately — tokens never appear in tool
arguments or transcripts. `GSHEETS_API_BASE` overrides the endpoint for tests
and proxies. Token acquisition and refresh belong to the embedding platform;
sheetd never runs an OAuth flow.

## How push works

At pull time the adapter records a **baseline**: the entered content
(formula or literal) of every cell, plus each sheet's remote id and grid
size. `sheet_save` diffs current content against that baseline and emits:

- `updateCells` for changed and new cells, grouped into per-row runs.
  Formulas push as `formulaValue`, numbers/bools/text as their typed values;
  date-formatted numbers carry a `numberFormat` so they render as dates
  remotely (serials share the same epoch on both sides).
- empty `CellData` under a `userEnteredValue` field mask for cleared cells,
- `updateSheetProperties` grid resizes when local data outgrew the remote grid,
- `addSheet` (with client-chosen ids) for sheets created locally.

After a successful push the baseline is fast-forwarded, so repeated saves are
incremental. A save with no changes makes no API call at all.

## Conflict policy (v1)

Last-write-wins, with a tripwire: before pushing, a light properties-only
fetch compares the remote sheet structure against the pull-time baseline and
a warning is attached to the result if the remote changed underneath you.
Cell-level remote edits are not detected — the diff is against the pull-time
baseline, so untouched cells are never written and remote edits to *other*
cells survive; concurrent edits to the *same* cell lose to the push.

## Known edges

- Sheets deleted locally are **not** deleted remotely (warned instead).
- Renaming a sheet locally makes it look like a new sheet at push time
  (`addSheet`) — the remote original stays; rename remotely instead.
- Charts, pivots, and rich formatting are untouched by push (only cells the
  session changed are written), but they do not exist in the local model.
- Formulas the local engine cannot evaluate (`#N/IMPL!`, `#NAME?`) keep
  their original text and push back unchanged unless edited; the pull
  sketch warns with a count.
- Workbooks rehydrated from the server blob store lose their push baseline;
  `sheet_save` says so and asks for a fresh pull.
