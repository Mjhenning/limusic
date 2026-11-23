# CONTROL API and integrations

Limusic can expose a small local HTTP API for scripts, bots, and dashboards, and
can push the current song to Tuna for OBS. Both are optional and are configured
in **Settings → Integrations**.

This document describes the fork's API contract. The API is local-first: it is
off by default, has no CORS support, and is not meant to be a public web service.

## Enable the API

1. Open **Settings → Integrations → Control API**.
2. Turn on **Enable Control API**. The server starts immediately.
3. Choose a bind mode:
   - **This PC only** (default): `127.0.0.1`. Nothing else on the network can
     reach the API.
   - **Local network**: `0.0.0.0`. Any device that can reach this machine and
     knows the token can control playback. Windows may need a firewall rule
     for the port.
4. Check **Connection address** for the URL to use. With local network mode,
   detected private IPv4 addresses are shown as separate `http://…` links.
5. Copy the token from the masked token field. Regenerating it disconnects all
   existing HTTP clients and WebSocket sockets immediately.

Defaults:

| Setting | Default |
| --- | --- |
| Enabled | `false` |
| Bind | `127.0.0.1` |
| Port | `27199` |
| Token | generated once, 64 lowercase hex characters |

The token is only readable from the Settings UI. It is never accepted in a
query string, never returned by the HTTP API, and never logged.

## Authentication and request rules

Every request, including `GET /api/v1/ws`, needs the header:

```http
Authorization: Bearer <TOKEN>
```

Additional rules:

- `?token=…` is **not** accepted. A query token without the header is a `401`.
- POST routes that take a body require `Content-Type: application/json`.
  A missing or different content type returns `415 unsupported_media_type`.
- JSON bodies are limited to 1 MiB.
- No CORS headers are added. Browser pages on other origins cannot call the
  API; use a script, bot, or proxy you control.
- The API is read-only over WebSocket. All mutations go through HTTP.

## Responses

Successful responses are JSON. There is no separate envelope.

Errors always use:

```json
{
  "error": "busy",
  "message": "another control request is still mutating the player; retry after 10 seconds"
}
```

Optional extra fields appear on specific errors: `current` (`next_timeout`),
`position` and `matched` (`duplicate`), and `duration_sec` /
`max_duration_sec` (`too_long`).

`busy` also sets the HTTP header `Retry-After: 10`. Prefer that header over
parsing the message.

### Error codes

| HTTP | `error` | Meaning |
| --- | --- | --- |
| 400 | `invalid_input` | Bad field, malformed JSON/query, or unsupported `from` value |
| 400 | `invalid_position` | Seek target is non-finite, negative, or past the track end |
| 400 | `invalid_volume` | Volume is not an integer from 0 to 100 |
| 400 | `invalid_index` | Queue index is out of range or points at the current row |
| 400 | `unsupported_url` | Request input is not a YouTube URL/id/query |
| 400 | `playlist_not_supported` | Request input was a playlist |
| 401 | `unauthorized` | Missing or wrong bearer token |
| 404 | `not_found` | Unknown route, or no song matched the request |
| 405 | `method_not_allowed` | Known route, wrong HTTP method |
| 409 | `listen_together_guest` | A Listen Together host controls playback |
| 409 | `remote_linked` | Another linked device is the player |
| 409 | `queue_changed` | A stale `video_id` was sent to `/queue/remove` |
| 409 | `duplicate` | Song is already queued; counterparts count as duplicates |
| 415 | `unsupported_media_type` | Body route without `application/json` |
| 422 | `duration_unknown` | A duration limit was set but the track's length is unknown |
| 422 | `too_long` | Track exceeds `max_duration_sec` |
| 422 | `explicit_content` | Track is explicit and `reject_explicit` was set |
| 500 | `playback_error` | Player operation failed |
| 503 | `busy` | Mutation lock waited 10 s; retry later |
| 503 | `too_many_clients` | Eight WebSocket clients are already connected |
| 504 | `next_timeout` | Skip did not become current within 15 s |
| 504 | `resolve_timeout` | Song resolution did not finish within 12 s |
| 502 | `resolve_failed` | Upstream resolver failed |

## Track shape

Queue items, `now`, and WebSocket `current`/`next`/`previous` use the same slim
track shape. Library tokens, play counts, ratings, and `added_at` are never
exposed.

```json
{
  "video_id": "dQw4w9WgXcQ",
  "title": "Song",
  "artists": "Artist",
  "artist_list": ["Artist"],
  "album": "Album",
  "album_id": "browse-id",
  "duration": "3:05",
  "duration_sec": 185,
  "thumbnail": "https://i.ytimg.com/vi/dQw4w9WgXcQ/hqdefault.jpg",
  "explicit": false,
  "is_video": false,
  "live": false,
  "queued_by": "bot",
  "queued": false,
  "queued_end": false
}
```

Optional metadata may be `null`. `duration_sec` is seconds; `duration` is the
display string.

## HTTP endpoints

All paths are under the API base URL, for example `http://127.0.0.1:27199`.

### Read state

#### `GET /api/v1/status`

Full playback snapshot plus device context. Never includes the token.

```json
{
  "now": { "video_id": "…", "title": "…" },
  "paused": false,
  "position": 42.5,
  "duration": 185.0,
  "volume": 80,
  "speed": 1.0,
  "semitones": 0,
  "reverb": {},
  "linked": false,
  "remote_linked": false,
  "listen_together_role": "none"
}
```

`position` and `duration` are seconds. `linked`, `remote_linked`, and
`listen_together_role` report who controls playback. `live` is only present
for a remote broadcast.

#### `GET /api/v1/now-playing`

Current track plus the few facts a player bar needs.

```json
{
  "now": { "video_id": "…", "title": "…" },
  "paused": false,
  "position": 42.5,
  "duration": 185.0,
  "live": false
}
```

#### `GET /api/v1/queue`

Defined queue projection. UI-only fields such as `currentIndex`, `prevTrack`,
and `sourceName` are absent.

Query parameters:

| Parameter | Meaning |
| --- | --- |
| `limit` | Maximum number of rows in `items` |
| `offset` | Full-queue index where the returned window starts |
| `from=current` | Use the current row's index as `offset`; an explicit `offset` wins |

Any other `from` value returns `400 invalid_input`.

```json
{
  "items": ["ApiTrack"],
  "current_index": 3,
  "shuffle": false,
  "repeat": "off",
  "autoplay": true,
  "next": { "video_id": "…" },
  "previous": { "video_id": "…" },
  "next_index": 4,
  "previous_index": 2,
  "next_is_autoplay_fetch": false,
  "total": 20,
  "offset": 0
}
```

`total` and all indices describe the full queue. Only `items` is paged, and
`offset` echoes the full-queue start index.

### Playback

All playback POSTs take no body. They return `{"ok": true, …}`.

| Endpoint | Extra response fields | Notes |
| --- | --- | --- |
| `POST /api/v1/playback/play` | `playing: bool` | Idempotent; starts a restored queue |
| `POST /api/v1/playback/pause` | `playing: bool` | Idempotent |
| `POST /api/v1/playback/toggle` | `playing: bool` | Reports the final state |
| `POST /api/v1/playback/next` | `current: ApiTrack` | 15 s deadline; see [Timing](#timing-and-retries) |
| `POST /api/v1/playback/previous` | `restarted: bool`, `current: ApiTrack` | `restarted` means the 3 s rewind rule fired |

`POST /api/v1/playback/seek`:

```json
{ "position_sec": 42.5 }
```

`position` is accepted as an alias for older clients. The value must be
finite, non-negative, and no greater than the current duration. Response:
`{"ok": true, "position_sec": 42.5}`.

`POST /api/v1/playback/volume`:

```json
{ "volume": 80 }
```

Volume is an integer from 0 to 100. Response: `{"ok": true, "volume": 80}`.

### Queue edits

`POST /api/v1/queue/remove`:

```json
{ "index": 4, "video_id": "dQw4w9WgXcQ" }
```

`index` is zero-based and must be an upcoming row. `video_id` is optional;
when present, the row must still match or the request returns
`409 queue_changed`. Response: `{"ok": true}`.

`POST /api/v1/queue/clear-queued` takes no body and returns `{"ok": true}`.
It clears queued/play-next rows without touching history or autoplay filler.

### Queue requests

`POST /api/v1/queue/request` resolves one YouTube URL, 11-character video id,
or text query and inserts it.

```json
{
  "input": "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
  "requested_by": "bot",
  "max_duration_sec": 360,
  "allow_duplicates": false,
  "play_next": true,
  "allow_unknown_duration": false,
  "reject_explicit": false
}
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `input` | string | required | URL, video id, or search query |
| `requested_by` | string? | `null` | Stored as `queued_by` |
| `max_duration_sec` | integer? | `null` | Reject tracks longer than this |
| `allow_duplicates` | bool | `false` | Skip this request's duplicate check |
| `play_next` | bool? | `true` | Insert into the Play-next block; `false` appends before autoplay |
| `allow_unknown_duration` | bool | `false` | Accept unknown length when a limit is set |
| `reject_explicit` | bool | `false` | Refuse explicit tracks |

Successful response:

```json
{
  "ok": true,
  "position": 4,
  "item": { "video_id": "…", "title": "…" }
}
```

`position` is the final zero-based index of the inserted row. `item` describes
the exact version that will play.

`POST /api/v1/queue/add` and `POST /api/v1/queue/play-next` are thin variants
that accept only `input` and `requested_by`. `/add` forces `play_next: false`;
`/play-next` forces `play_next: true`. Both use all other defaults.

Resolution is limited to 12 s. On timeout the response is `504 resolve_timeout`
and nothing is inserted. See [Timing](#timing-and-retries).

### WebSocket

`GET /api/v1/ws` upgrades to a read-only event stream. The bearer header is
checked before the upgrade; a missing or wrong token returns the normal `401`
JSON envelope, not `101`.

- At most **eight** sockets may be open. A ninth gets `503 too_many_clients`.
- The first frame is `hello` with a full snapshot.
- If the server's event hub drops frames for a client, the next frame is
  `resync` with a fresh full snapshot.
- Incoming frames are ignored. Pongs are handled by the WebSocket layer.
- Sockets are closed when the token is regenerated, the API is disabled or
  rebound, or the app exits.

Every frame has a `type` field.

#### `hello` and `resync`

Both carry the same fields:

```json
{
  "type": "hello",
  "current": { "video_id": "…" },
  "position_sec": 42.5,
  "duration_sec": 185.0,
  "playing": true,
  "volume": 80,
  "shuffle": false,
  "repeat": "off",
  "current_index": 3,
  "total": 20,
  "next": { "video_id": "…" },
  "previous": { "video_id": "…" },
  "next_is_autoplay_fetch": false,
  "listen_together_role": "none"
}
```

`repeat` is `"off"`, `"all"`, or `"one"`. `listen_together_role` is `"none"`,
`"host"`, or `"guest"`. Positions and durations are seconds.

#### Other frames

| `type` | Fields |
| --- | --- |
| `now_playing` | `current: ApiTrack`, optional `position_sec`, optional `duration_sec` |
| `playback_state` | `playing: bool`, `state: string` |
| `queue_changed` | `current_index: number?`, `total: number` — refetch `/queue` for rows |
| `shuffle_changed` | `shuffle: bool` |
| `repeat_changed` | `repeat: string` |
| `volume` | `volume: number` |
| `playback_error` | `message: string`, optional `video_id: string` |
| `position` | `position_sec: number` (~1 Hz while playing, plus discrete seeks/pauses) |

Continuous player ticks are not mirrored from the event hub; `position` frames
are generated per connection from the authoritative snapshot.

## Timing and retries

These limits are part of the API contract.

### Mutation lock and `503 busy`

One lock serializes changes to the current track and queue: `/playback/next`,
`/playback/previous`, `/queue/remove`, `/queue/clear-queued`, and the insert
phase of the three queue-request endpoints. A handler waits at most **10 s**.

If the wait expires, the response is `503` with `error: "busy"` and
`Retry-After: 10`. Honor that header; do not immediately retry.

Network resolution for queue requests happens **before** the lock is taken, so
a slow resolver cannot block `play`, `pause`, `volume`, or other mutations.

### Skip deadline

`POST /api/v1/playback/next` waits up to **15 s** for the current queue row to
change, including a synchronous autoplay fetch. The wait ends when the queue's
current index or video id changes, not when audio starts.

The skip is never cancelled at the HTTP deadline: it keeps the mutation lock
until it finishes. There is a hard **45 s** ceiling at which a stalled skip is
cancelled, the previous row is restored, and the lock is released.

On deadline the response is `504` with `error: "next_timeout"` and `current`
set to the track that was current when the request began. That response does
**not** prove the skip failed.

> Use an HTTP client timeout of **at least 20 s** for `/playback/next`. On a
> timeout or `next_timeout`, re-read `/api/v1/status` before retrying.

### Resolve timeout

`/queue/request`, `/queue/add`, and `/queue/play-next` each wrap song
resolution in a **12 s** timeout. The shared InnerTube client's own connect and
total timeouts are unchanged; this is a control-API-specific ceiling.

A timeout returns `504` with `error: "resolve_timeout"` ("song resolution did
not finish within 12 s"). No queue row is inserted. Retrying the same input is
safe, but check `409 duplicate` in case an earlier attempt completed.

## Examples

Replace `<TOKEN>` with the token from Settings. Do not paste real tokens into
scripts that are committed to source control or printed in logs.

### curl

```sh
API=http://127.0.0.1:27199
AUTH="Authorization: Bearer <TOKEN>"

curl -sS -H "$AUTH" "$API/api/v1/status"
curl -sS -H "$AUTH" "$API/api/v1/now-playing"
curl -sS -H "$AUTH" "$API/api/v1/queue?from=current&limit=10"

curl -sS -X POST -H "$AUTH" "$API/api/v1/playback/toggle"
curl -sS -X POST -H "$AUTH" "$API/api/v1/playback/volume" \
  -H 'Content-Type: application/json' \
  -d '{"volume":70}'
curl -sS -X POST -H "$AUTH" "$API/api/v1/playback/seek" \
  -H 'Content-Type: application/json' \
  -d '{"position_sec":30}'
curl -sS -X POST -H "$AUTH" "$API/api/v1/queue/request" \
  -H 'Content-Type: application/json' \
  -d '{"input":"never gonna give you up","requested_by":"curl","max_duration_sec":300}'
```

### Node.js

No third-party packages are needed for HTTP. This example uses Node's built-in
`fetch`, honors `Retry-After`, and uses the recommended 20 s timeout for
`/playback/next`.

```js
const base = 'http://127.0.0.1:27199';
const token = process.env.LIMUSIC_CONTROL_TOKEN; // never hard-code it
const headers = {
  Authorization: `Bearer ${token}`,
  'Content-Type': 'application/json'
};

async function api(path, options = {}, timeoutMs = 10_000) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const res = await fetch(base + path, {
      ...options,
      headers: { ...headers, ...options.headers },
      signal: controller.signal
    });
    const body = await res.json().catch(() => null);
    if (!res.ok) {
      const err = new Error(body?.message ?? `HTTP ${res.status}`);
      err.code = body?.error;
      err.retryAfter = Number(res.headers.get('retry-after') ?? 0);
      throw err;
    }
    return body;
  } finally {
    clearTimeout(timer);
  }
}

async function next() {
  try {
    const res = await api('/api/v1/playback/next', { method: 'POST' }, 20_000);
    console.log('now playing:', res.current.title);
  } catch (err) {
    if (err.code === 'busy') {
      await new Promise((r) => setTimeout(r, err.retryAfter * 1000));
      return next();
    }
    if (err.code === 'next_timeout') {
      const status = await api('/api/v1/status');
      console.log('skip may still have completed:', status.now?.title);
      return;
    }
    throw err;
  }
}

api('/api/v1/now-playing').then((now) => console.log(now));
next();
```

For WebSocket clients, use a client that can send the `Authorization` header
on the upgrade request. Node 22's built-in `WebSocket` accepts headers:

```js
const ws = new WebSocket('ws://127.0.0.1:27199/api/v1/ws', {
  headers: { Authorization: `Bearer ${token}` }
});
ws.addEventListener('message', (event) => {
  const frame = JSON.parse(event.data);
  console.log(frame.type, frame);
});
```

## Tuna / OBS output

Tuna is an OBS browser-source companion. Limusic is the **client**: it POSTs
playback state to Tuna's local HTTP listener. It does not open or control OBS.

### Setup

1. Install and start Tuna, and add its browser source to OBS. Note the URL its
   listener uses; the default is `http://127.0.0.1:1608/`.
2. In Limusic, open **Settings → Integrations → OBS / Tuna**.
3. Turn on **Enable Tuna output** and set **Tuna URL** to that exact endpoint.
   No path is appended; use any path or query Tuna requires verbatim.
4. Press **Send test**. Success shows a fresh "Last successful push"; failure
   shows the HTTP/transport error in the panel.
5. Play a song. The OBS source should pick up the track and artwork.

If Tuna runs on another machine, use a reachable `http(s)://host:port/...` URL.
The `hostname` field in each payload identifies the Limusic machine. Tuna's own
listener is not authenticated by Limusic; protect it at the network level.

### Payload

Each push is a JSON POST with a 3 s request timeout:

```json
{
  "data": {
    "cover_url": "https://i.ytimg.com/vi/…/hqdefault.jpg",
    "title": "Song",
    "artists": ["Artist"],
    "status": "playing",
    "progress": 12345,
    "duration": 180500,
    "album_url": "https://music.youtube.com/browse/album123",
    "album": "Album"
  },
  "hostname": "this-pc",
  "date": 1763880000000
}
```

| Field | Type | Source |
| --- | --- | --- |
| `data.cover_url` | string? | Queue row thumbnail, passed through unchanged. Kept on pause. |
| `data.title` | string | Track title; `""` when no track |
| `data.artists` | string[] | `artist_list`, or the artist string wrapped in a one-item array |
| `data.status` | `"playing"` or `"stopped"` | See below |
| `data.progress` | integer ms | Playback position × 1000, truncated |
| `data.duration` | integer ms | Track length × 1000, truncated |
| `data.album_url` | string? | `https://music.youtube.com/browse/<album_id>` |
| `data.album` | string? | Album name |
| `hostname` | string | Machine hostname, or `"limusic"` if unavailable |
| `date` | integer ms | Unix epoch milliseconds |

There is no `"paused"` value. A paused Limusic track is reported as
`"stopped"` with its metadata, including `cover_url`, retained. No periodic
pushes are sent while paused or with no track. One immediate stopped push is
sent when playback pauses so Tuna does not keep showing a stale playing card.

For local files, `cover_url` can be a local path rather than an HTTP URL.

### When pushes happen

- Immediately when the current track or playback state changes, or after a hub
  resync.
- Immediately after enabling the output or changing its URL.
- While playing, about once per second.
- On **Send test**, which forces one attempt regardless of playback state.

Payloads are never logged. Failures set the status error and start backoff:
`1, 2, 4, 8, 16, 30` seconds, capped at 30. A success clears the error and
backoff. Forced/test pushes also respect an active backoff window.

### Status fields

The Integrations panel polls `get_integration_status` about every 2 s while
visible. The Tuna part is:

| Field | Meaning |
| --- | --- |
| `enabled` | Saved on/off state |
| `url` | Saved, trimmed target URL |
| `last_ok_at_ms` | Epoch ms of the last successful push, or `null` |
| `last_error` | Last transport/HTTP error, or `null` |
| `backoff_secs` | Current retry delay in seconds, or `null` when healthy |

Changing settings applies immediately; no app restart is required. Disabling
or rebinding the Control API does not stop Tuna, but app exit does.

## Security notes

- Keep the token out of URLs, logs, screenshots, and committed scripts.
- Prefer **This PC only** unless another device genuinely needs access.
- Local network mode still requires the bearer token; it does not make the API
  public, but it does expose it to anyone on that network who can guess or
  steal the token.
- Regenerate the token if it leaks. This disconnects every WebSocket and
  invalidates every existing client immediately.
- The API has no CORS support by design. A browser page cannot silently drive
  the player from another origin.
