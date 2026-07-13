# `sheets.channel.v1` — the realtime workbook channel

`GET /workbooks/{id}/channel` upgrades to a WebSocket. All messages are JSON
objects with a `type` field. The design goal: a UI can mirror the workbook
live — including *what an agent is doing to it* — with nothing but this
channel plus one file fetch.

## Authority model

The server holds the one authoritative engine session per workbook; every
mutation from every door (stdio MCP, HTTP MCP, REST, this channel) is
serialized through it and fans out here with a per-workbook monotonic `seq`.
Clients are replicas: apply `applied` frames in `seq` order. On a `gap` frame
(slow consumer) or a `seq` discontinuity, resync: `GET /workbooks/{id}/file?format=ic`,
reload, resubscribe.

## Server → client

| type | fields | meaning |
|---|---|---|
| `welcome` | `v`, `workbook_id`, `engine_version`, `seq`, `sheets[]`, `highlights[]` | sent on connect and on `hello` |
| `applied` | `seq`, `principal`, `cmd_id?`, `ok`, `summary`, `delta[[addr,old,new],…]` (capped), `delta_total`, `diffs_b64` | a script ran and changed state |
| `rejected` | `principal`, `cmd_id?`, `line`, `error` | a script line failed (earlier lines may still have applied — watch for a paired `applied`) |
| `agent.status` | `principal`, `phase: executing\|idle`, `script_line?` | exec lifecycle, drives "the agent is working" UI |
| `presence` | `principal`, `joined?`/`left?`/`selection?`/`viewport?`/`editing_cell?` | fan-out of peer presence |
| `highlight.set` / `highlight.clear` | `id`, `range`, `color`, `note?`, `author` | the shared pointing finger |
| `gap` | `missed` | you lagged; resync |
| `error` | `error` | direct reply to a bad client message |

## Client → server

| type | fields | effect |
|---|---|---|
| `hello` | — | re-request `welcome` |
| `cmd` | `id?`, `script` | run a DSL script; results come back as `applied`/`rejected` fan-out |
| `presence` | `selection?`, `viewport?`, `editing_cell?` | broadcast to peers |
| `highlight.set` | `range`, `color?`, `note?` | create a highlight |
| `highlight.clear` | `id` | remove one |

## `diffs_b64`

The base64 payload in `applied` is the engine's own diff-queue blob
(`UserModel::flush_send_queue`). A replica running the **exact same engine
version** (see `engine_version` in `welcome`) applies it with
`applyExternalDiffs`/`apply_external_diffs` for cell-perfect mirroring without
refetching. Any version mismatch ⇒ treat as opaque, rely on `delta` +
resyncs instead. The blob is empty for runs that changed nothing.

## Auth & identity

Static bearer token via `Authorization: Bearer …` or `?token=` (WebSocket
clients in browsers can only use the query form). The presence identity comes
from `?principal=` or the configured principal header. Permissions are
all-or-nothing per token in this version.
