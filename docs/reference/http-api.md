# HTTP API

A read/write API on the agent, off unless configured:

```yaml
server:
  http_listen: "127.0.0.1:7441"
  http_token: "a-secret"
```

The same rule as TCP applies: **a non-loopback address requires a token**, or
the agent refuses to start. Present it as `Authorization: Bearer <token>`.

Not a mount transport. Mounts speak the binary protocol; this exists for
dashboards, scripts and CI. For JavaScript there is a typed client —
[alloyfs-http](https://http.alloy.okyle.dev) — which wraps everything below.

## Endpoints

### Read

| Method | Path | Purpose |
|---|---|---|
| GET | `/api/status` | Version and the exports served |
| GET | `/api/exports` | Export list |
| GET | `/api/exports/{name}/browse?path=&cursor=&limit=` | Directory listing, **paged** |
| GET | `/api/exports/{name}/stat?path=` | One path's attributes |
| GET | `/api/exports/{name}/statfs` | Capacity of the export's volume |
| GET | `/api/exports/{name}/readlink?path=` | A symlink's stored target |
| GET | `/api/exports/{name}/file?path=` | File contents (supports `Range`) |
| GET | `/api/exports/{name}/events` | Server-sent events |

### Write

| Method | Path | Purpose |
|---|---|---|
| POST | `/api/exports/{name}/file?path=[&offset=]` | Write the body, whole or at an offset |
| POST | `/api/exports/{name}/mkdir?path=` | Create a directory |
| POST | `/api/exports/{name}/delete?path=[&recursive=true]` | Delete a path |
| POST | `/api/exports/{name}/rename` | `{"from":…,"to":…}` — atomic move |
| POST | `/api/exports/{name}/copy` | `{"from":…,"to":…}` — files only |
| POST | `/api/exports/{name}/setattr?path=` | `{"mode":…,"mtime_ms":…}` |
| POST | `/api/exports/{name}/bulk` | `{"op":…,"paths":[…]}` — per-path results |

`GET` routes also answer `HEAD`, with the body removed.

## Listing a directory

`browse` is paged. Without `limit` a page is 1024 entries — the same page size
the binary protocol's readdir uses, so the two surfaces behave alike.

```json
{
  "entries": [
    { "name": "src", "kind": "dir", "size": 4096,
      "mtime_ms": 1756000000000, "version": 41, "mode": 493 }
  ],
  "next_cursor": 1024,
  "total": 20481
}
```

Feed `next_cursor` back as `?cursor=`; `null` means that was the last page.
`total` is the whole directory, so a caller can size a progress bar without
walking to the end first.

`kind` is `"file"`, `"dir"` or `"symlink"`. `mode` is POSIX mode bits.

## Reading a file

`GET /file` serves a single `Range`, a weak `ETag` and `Last-Modified`, so
resumed downloads and polling both work:

```bash
curl -H "Authorization: Bearer $TOKEN" \
  -H "Range: bytes=0-1023" \
  "http://127.0.0.1:7441/api/exports/projects/file?path=big.bin"
```

Multi-range requests are not supported; per RFC 9110 `Range` is advisory, so
the whole file comes back instead of an error. An unsatisfiable single range
is a **416** with `Content-Range: bytes */<len>`.

Every response carries `X-Content-Type-Options: nosniff` and
`Content-Security-Policy: sandbox`, and `.html` is served as `text/plain`.
Files that people uploaded must render, but must never script against this
origin.

## Writing safely

The ETag from a `GET` is accepted back on a write, which is what makes
read-modify-write safe against a second client:

```bash
# Only overwrite if nothing changed since we read it.
curl -X POST -H "Authorization: Bearer $TOKEN" \
  -H 'If-Match: W/"a-1993f4c8f1e"' \
  --data-binary @local.txt \
  "http://127.0.0.1:7441/api/exports/projects/file?path=notes.txt"
```

- `If-Match: <etag>` — write only if the file still has that ETag, else **412**.
- `If-Match: *` — write only if the path exists.
- `If-None-Match: *` — write only if it does **not** exist. Claim-if-absent.

A refused precondition writes nothing.

### Files larger than one request

A request body is capped at 256 MiB. `offset=` writes the body at a byte
offset instead of replacing the file, so a larger file is uploaded in chunks:

```bash
curl -X POST --data-binary @part0 ".../file?path=big.bin"
curl -X POST --data-binary @part1 ".../file?path=big.bin&offset=8388608"
```

The trade is real and worth knowing: a whole-file write lands **atomically**
via a same-directory temp file and rename, so a concurrent reader sees either
the old content or the new one. An `offset` write lands **in place**, so a
reader during a multi-chunk upload can see a partially written file. Use the
whole-file form unless the file is too big for it.

## Bulk

One request, one result per path, and a failure on any one path does not fail
the others or the request:

```bash
curl -X POST -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"op":"stat","paths":["a.txt","missing.txt"]}' \
  "http://127.0.0.1:7441/api/exports/projects/bulk"
```

```json
[
  { "path": "a.txt", "ok": true, "entry": { "name": "a.txt", "kind": "file", "size": 12, "…": null } },
  { "path": "missing.txt", "ok": false, "error": "not_found" }
]
```

`op` is `stat`, `delete` or `mkdir`. At most 10 000 paths per request.

**Check the per-item `ok`, not the status code** — a mixed batch is still a
`200`. The response is only non-2xx when the request itself was malformed.

`error` is a stable string: `not_found`, `permission_denied`, `invalid_path`,
`not_a_directory`, `is_a_directory`, `already_exists`, `not_empty`,
`read_only`, `io`.

## Events

`GET /events` is a Server-Sent-Events stream. The standard `Last-Event-ID`
reconnect header maps to the agent's ring log for catch-up; a resume that is
too old gets a `resync` event rather than silently missing changes.

```bash
curl -N -H "Authorization: Bearer $TOKEN" \
  http://127.0.0.1:7441/api/exports/projects/events
```

## Status codes

| Code | Means |
|---|---|
| 400 | Not a directory / is a directory, or a malformed request |
| 401 | Missing or invalid bearer token |
| 403 | Refused: traversal, or a write to a `read_only` export |
| 404 | Not found — **and what an excluded path returns** |
| 409 | Already exists, or a non-empty directory without `recursive` |
| 412 | A precondition (`If-Match` / `If-None-Match`) failed |
| 413 | Body over 256 MiB, or over 10 000 bulk paths |
| 416 | Range not satisfiable |

## Behaviour worth relying on

The API goes through the same hardening as the wire protocol, not a second
implementation:

- Path traversal is refused, including a destination that would escape via a
  symlink.
- Excluded paths return **404**, never 403 — existence does not leak. This
  holds for `stat`, `browse`, `bulk` and every write route alike.
- A `read_only` export refuses writes here too.
- Writes are origin-less on purpose: origin tagging exists so a mounted
  session does not hear its own writes echoed back, and an HTTP client has no
  session to suppress. Every mount sees these changes through the watcher.
