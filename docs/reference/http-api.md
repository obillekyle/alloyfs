# HTTP API

A small read/write API on the agent, off unless configured:

```yaml
agent:
  http_listen: "127.0.0.1:7441"
  http_token: "a-secret"
```

The same rule as TCP applies: **a non-loopback address requires a token**, or
the agent refuses to start. Present it as `Authorization: Bearer <token>`.

## Endpoints

| Method | Path | Purpose |
|---|---|---|
| GET | `/api/status` | Version and the exports served |
| GET | `/api/exports` | Export list |
| GET | `/api/exports/{name}/browse?path=` | Directory listing |
| GET | `/api/exports/{name}/file?path=` | File contents |
| POST | `/api/exports/{name}/file?path=` | Write a file (body is the content) |
| POST | `/api/exports/{name}/mkdir?path=` | Create a directory |
| POST | `/api/exports/{name}/delete?path=` | Delete a path |
| GET | `/api/exports/{name}/events` | Server-sent events |

## Examples

```bash
curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:7441/api/status
curl -H "Authorization: Bearer $TOKEN" \
  "http://127.0.0.1:7441/api/exports/projects/browse?path=src"
curl -N -H "Authorization: Bearer $TOKEN" \
  http://127.0.0.1:7441/api/exports/projects/events
```

## Behaviour worth relying on

The API goes through the same hardening as the wire protocol, not a second
implementation:

- Path traversal is refused.
- Excluded paths return **404**, never 403 — existence does not leak.
- A `read_only` export refuses writes here too.
