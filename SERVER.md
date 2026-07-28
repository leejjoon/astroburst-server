# AstroBurst Headless Server

`astroburst-server` is a standalone HTTP server that exposes the full AstroBurst imaging pipeline over REST. It is designed to run on a remote GPU workstation and be accessed through an SSH tunnel by a local agent or script.

---

## Building

```bash
cargo build --release \
  --no-default-features \
  --features server,astrometry-net,asdf-full,vizier
```

The binary is at `target/release/astroburst-server`.

For development (faster compile, no optimisations):

```bash
cargo run --bin astroburst-server \
  --no-default-features \
  --features server,astrometry-net,asdf-full,vizier
```

---

## Running

### Local

```bash
./target/release/astroburst-server
```

Startup log:

```
INFO  astroburst_server] AstroBurst Headless Server v0.5.3
INFO  astroburst_server] Listening on 127.0.0.1:8097
INFO  astroburst_server] Session TTL: 900s, Max sessions: 8
INFO  astroburst_server] Per-session cache: 32 entries / 2048 MiB
INFO  astroburst_server] Cleanup interval: 60s
```

### Command-line interface

The binary takes **no positional flags** — runtime configuration is entirely via the `ASTROBURST_*` environment variables below. `argv` is used only to select a mode:

| Invocation | What it does |
|---|---|
| `astroburst-server` | Start the HTTP server |
| `astroburst-server connect <target>` | Open and supervise an SSH tunnel to a remote server (see below) |
| `astroburst-server tui [URL]` | Run the live terminal dashboard against a server (default `http://127.0.0.1:8097`) |
| `astroburst-server --help` / `-h` | Print usage and exit |
| `astroburst-server --version` / `-V` | Print the version and exit |

Any unrecognised argument — a stray flag, or a positional such as a hostname meant for `connect` — prints usage to stderr and exits `2`. The server takes no positional args, so it starts only when invoked with **no** arguments; it never falls through and boots silently on a typo.

### Remote GPU workstation via SSH tunnel

The server binds loopback-only (`127.0.0.1:8097`) for security, so a remote instance isn't reachable directly. Two ways to reach it from your local machine:

**Recommended — the `connect` subcommand** manages the tunnel for you: it execs your system `ssh` (so `~/.ssh/config` aliases, keys, ssh-agent and jump hosts all work), forwards a local port to the remote's loopback port, probes `/health`, prints a working local URL, and auto-reconnects with backoff if the link drops.

```bash
# On your local machine (server already running on the remote's :8097):
astroburst-server connect gpubox            # ~/.ssh/config alias or hostname
astroburst-server connect ssh://user@host.example:2222   # :2222 is the SSH port
```

It prints the local URL to use:

```
connect: tunnel up -- server URL: http://127.0.0.1:<local-port>
```

Leave it running to hold the tunnel open. Key flags:

| Flag | Meaning | Default |
|---|---|---|
| `--remote-port N` | Server's HTTP port on the remote host | `8097` |
| `--local-port N` | Local port for the forward | auto-picks a free port |
| `--start` | Launch the remote server over SSH if nothing is listening | off |
| `--remote-bin PATH` | Binary that `--start` runs on the remote | `astroburst-server` (on `$PATH`) |
| `--tui` | Run the live dashboard instead of the plain supervisor | off |
| `--json` | Emit one machine-readable line (`{"url":…,"local_port":…,"health":…}`) for scripts | off |
| `--no-reconnect` | Exit when the tunnel drops instead of respawning | reconnects |
| `--max-retries N` | Cap reconnect attempts | unlimited |

`--json` and `--tui` are mutually exclusive. `astroburst-server connect --help` prints the full list.

#### Launching the remote server with `--start`

`--start` only launches a server when the target port is **not** already serving — it probes `/health` first:

- **A server is already running there** → `--start` is a no-op; `connect` uses the existing server. Passing `--start` defensively is safe and idempotent.
- **Nothing is listening** → the server is launched over SSH as `astroburst-server` (or `--remote-bin PATH`), bound to `127.0.0.1:<remote-port>` via `ASTROBURST_BIND`, detached with `nohup`, and logging to `${TMPDIR:-/tmp}/astroburst-server-<port>.log` on the remote. So the launched server's bind port is guaranteed to match what the tunnel forwards to.

The launch is **self-diagnosing** rather than silent-on-failure:

- If the binary isn't resolvable it says so and hints at `--remote-bin` with an absolute path (a non-login SSH shell often omits `~/.cargo/bin`, `~/.local/bin`).
- If the server dies on startup — e.g. the port is occupied by another process (`Error: Address in use (os error 98)`) — the tail of the remote log is echoed back to your terminal, followed by a fatal `--start failed to bring up astroburst-server …`. The occupying process is never touched.

When nothing is listening and `--start` is absent, `connect` exits with a clear message telling you to pass `--start` (or `--remote-port N` if the server is on a different port), instead of ssh's raw per-channel `Connection refused` noise (which is now filtered).

#### Version check

Once the tunnel is healthy, `connect` compares the remote server's reported `version` (from `/health`) against its own. If the `major.minor` differ it prints a warning to stderr (patch differences are treated as compatible; pre-1.0 minor bumps can be breaking). It's advisory only — the tunnel stays up:

```
connect: WARNING remote astroburst-server version 0.3.0 is incompatible with this client 0.2.1 (major.minor differ); the tunnel works but the API may not — rebuild/redeploy to match.
```

**Manual alternative** — set up the forward yourself:

```bash
# On the remote machine:
./astroburst-server

# On your local machine:
ssh -L 8097:localhost:8097 user@remote-host
```

All API calls then go to `http://localhost:8097` from the local machine — the SSH tunnel forwards them securely.

---

## Configuration

All knobs are set via environment variables. Unset variables use the defaults shown. An invalid value emits a warning and falls back to the default — the server never fails to start because of a bad env var.

| Variable | Default | Description |
|---|---|---|
| `ASTROBURST_BIND` | `127.0.0.1:8097` | TCP address to listen on |
| `ASTROBURST_SESSION_TTL` | `900` | Idle seconds before a session is evicted |
| `ASTROBURST_SESSION_MAX` | `8` | Maximum concurrent sessions |
| `ASTROBURST_JOBS_MAX` | `4` | Maximum concurrent CPU-bound jobs |
| `ASTROBURST_CACHE_MAX_ENTRIES` | `32` | Per-session image cache slot limit |
| `ASTROBURST_CACHE_MAX_BYTES` | `2147483648` | Per-session image cache memory limit (bytes) |
| `ASTROBURST_CLEANUP_INTERVAL` | `60` | Seconds between idle-session sweep runs |
| `ASTROBURST_LOG_LEVEL` | `info` | Log level (`trace`/`debug`/`info`/`warn`/`error`) |
| `RAYON_NUM_THREADS` | all logical cores | Threads for parallel CPU work — image compression/decompression and rendering |

`RUST_LOG` takes precedence over `ASTROBURST_LOG_LEVEL` when both are set.

`RAYON_NUM_THREADS` is a standard [rayon](https://docs.rs/rayon) variable, read once at startup, and is process-wide: it caps the thread pool shared by all parallel CPU work (e.g. RICE_1 compression on `POST /v2/.../export/compressed`, tile decompression, render pixel math). Set it to bound total CPU on a shared host; `RAYON_NUM_THREADS=1` forces fully sequential execution. Unset (the default) uses one thread per logical core.

Example — run with tighter limits and verbose logging:

```bash
ASTROBURST_SESSION_MAX=2 \
ASTROBURST_SESSION_TTL=300 \
ASTROBURST_LOG_LEVEL=debug \
./astroburst-server
```

---

## Session model

Every client starts with `POST /sessions` to obtain a `session_id`. All subsequent endpoints are scoped under `/sessions/:sid/`. A session holds:

- An **image cache** (LRU, configurable size) — loaded images live here under a slot key.
- A **job registry** — long-running operations (stacking, drizzle, pipeline) create a job and return immediately; the caller polls or streams progress.

Sessions expire after `ASTROBURST_SESSION_TTL` seconds of inactivity. Sessions with at least one running job are never evicted. The maximum number of concurrent sessions is `ASTROBURST_SESSION_MAX`; a `POST /sessions` when the cap is reached returns `503 Service Unavailable`.

---

## API reference

### Response format

All JSON responses follow this envelope:

```json
{ "success": false, "error": { "code": "not_found", "message": "slot ghost not in cache" } }
```

On success the envelope is absent — handlers return their data directly as the top-level JSON object.

Errors use standard HTTP status codes:

| Status | Code | Meaning |
|---|---|---|
| 400 | `bad_request` | Missing or invalid parameter |
| 404 | `not_found` | Session, slot, or job does not exist |
| 409 | `conflict` | SSE stream already has a subscriber |
| 429 | `too_many_requests` | All job semaphore slots occupied |
| 503 | `service_unavailable` | Session cap reached |
| 500 | `internal_error` | Unexpected server error |

Every response includes an `X-Request-Id` header (echoed from the request, or a generated UUID).

---

### Health

#### `GET /health`

Returns server status and session counts. No session required.

```json
{
  "status": "ok",
  "version": "0.5.3",
  "sessions_active": 2,
  "sessions_total": 17
}
```

---

### Sessions

#### `POST /sessions`

Create a new session.

**Response `201 Created`:**
```json
{ "session_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890" }
```

Returns `503` when `ASTROBURST_SESSION_MAX` is reached.

---

### FITS I/O

#### `POST /sessions/:sid/fits/open`

Load a FITS or ASDF file from the server's filesystem into the session cache.

**Body:**
```json
{
  "path": "/data/m51_ha.fits",
  "slot": "ha"
}
```

`slot` is the cache key used by subsequent endpoints. Defaults to `path` if omitted.

**Response `200`:**
```json
{
  "slot": "ha",
  "dims": [2048, 2048],
  "stats": { "min": 0.0, "max": 65535.0, "median": 412.3, "mean": 580.1, "sigma": 210.4, "mad": 98.2, "valid_count": 4194304 },
  "stf": { "shadow": 0.001, "midtone": 0.22, "highlight": 1.0 },
  "header": { "OBJECT": "M51", "FILTER": "Ha", "EXPTIME": 300.0 }
}
```

#### `POST /sessions/:sid/fits/header`

Return the full header for a slot already in the cache. Returns `404` if the slot was loaded without header data — re-open with `fits/open`.

**Body:**
```json
{ "slot": "ha" }
```

**Response `200`:**
```json
{
  "slot": "ha",
  "total_cards": 42,
  "cards": [{ "key": "OBJECT", "value": "M51" }, ...],
  "index": { "OBJECT": "M51", "FILTER": "Ha" }
}
```

---

### Rendering

All render endpoints return a raw PNG (`Content-Type: image/png`). The STF auto-render also sets `X-Stf-Params` on the response.

#### `POST /sessions/:sid/image/stf`

Auto-stretch the image using the Screen Transfer Function and return a PNG.

**Body:**
```json
{ "slot": "ha" }
```

#### `POST /sessions/:sid/image/render`

Render with explicit STF parameters.

**Body:**
```json
{ "slot": "ha", "shadow": 0.001, "midtone": 0.22, "highlight": 1.0 }
```

#### `POST /sessions/:sid/image/viewport`

Render a pixel-coordinate crop as a PNG.

**Body:**
```json
{ "slot": "ha", "x": 512, "y": 512, "w": 256, "h": 256 }
```

---

### Jobs

Long-running operations (stacking, drizzle, pipeline) return `202 Accepted` immediately with a `job_id`. Use these endpoints to track progress.

#### `GET /sessions/:sid/jobs/:jid`

Poll job status.

**Response `200`:**
```json
{
  "id": "...",
  "action": "stack",
  "status": "running",
  "pct": 42,
  "started_at": 1718000000000,
  "completed_at": null
}
```

`status` is one of: `running`, `done`, `error`, `cancelled`.

#### `DELETE /sessions/:sid/jobs/:jid`

Cancel a running job. No-op if already finished. Returns the same shape as GET.

#### `GET /sessions/:sid/jobs/:jid/stream`

Subscribe to real-time SSE progress. Events have a `type` field:

```
event: progress
data: {"type":"progress","pct":45,"stage":"aligning"}

event: complete
data: {"type":"complete"}

event: error
data: {"type":"error","message":"file not found: /data/bad.fits"}
```

Only one subscriber per job is allowed. A second `GET` to the same stream returns `409 Conflict`.

---

### Stacking

Both endpoints return `202 Accepted` and start a background job.

#### `POST /sessions/:sid/stacking/stack`

Sigma-clip stack a list of FITS files.

**Body:**
```json
{
  "paths": ["/data/ha_001.fits", "/data/ha_002.fits", "/data/ha_003.fits"],
  "result_slot": "ha_stacked",
  "sigma_low": 3.0,
  "sigma_high": 3.0,
  "max_iterations": 5,
  "align": true,
  "weights": [1.0, 1.0, 0.5]
}
```

All fields except `paths` are optional. `result_slot` defaults to `"stacked"`.
`weights` is an optional per-frame weight array (same length as `paths`); if the
lengths differ it is silently ignored and uniform weighting is used.

**Response `202`:**
```json
{ "job_id": "...", "status": "running", "slot": "ha_stacked" }
```

#### `POST /sessions/:sid/stacking/drizzle`

Drizzle-combine a list of FITS files.

**Body:**
```json
{
  "paths": ["/data/ha_001.fits", "/data/ha_002.fits"],
  "result_slot": "ha_drizzled",
  "scale": 2.0,
  "pixfrac": 0.7,
  "kernel": "lanczos3",
  "sigma_low": 3.0,
  "sigma_high": 3.0,
  "align": true
}
```

All fields except `paths` are optional. `result_slot` defaults to `"drizzled"`.
`kernel` is one of: `"square"` (default), `"gaussian"`, `"lanczos3"`.

**Response `202`:**
```json
{ "job_id": "...", "status": "running", "slot": "ha_drizzled" }
```

---

### Pipeline

#### `POST /sessions/:sid/pipeline/run`

Calibrate and stack one or more narrowband channels in a single pass. Builds calibration masters (bias, dark, flat) from supplied frames, then stacks each channel's lights. Returns `202 Accepted`.

**Body:**
```json
{
  "channels": [
    { "label": "Ha",   "paths": ["/data/ha_001.fits",   "/data/ha_002.fits"] },
    { "label": "OIII", "paths": ["/data/oiii_001.fits", "/data/oiii_002.fits"] }
  ],
  "bias_paths":  ["/data/bias_001.fits"],
  "dark_paths":  ["/data/dark_300s.fits"],
  "flat_paths":  ["/data/flat_ha.fits"],
  "sigma_low":   2.5,
  "sigma_high":  3.0,
  "normalize":   true,
  "result_prefix": "pipe/"
}
```

All calibration arrays and numeric fields are optional. `result_prefix` prepends a string to each channel label to form the cache slot key (e.g. `"pipe/Ha"`, `"pipe/OIII"`).

**Response `202`:**
```json
{ "job_id": "...", "status": "running", "slots": ["pipe/Ha", "pipe/OIII"] }
```

---

## v2 API

The `/v2/*` endpoints are a newer, agent-oriented surface for inspecting and
analysing a single image at a time. They share the session model and error
envelope above, but use their own **ref-based image model** instead of v1's
free-form cache slots. All v2 routes are prefixed `/v2/sessions/:sid/...`.

Session creation is shared with v1: `POST /sessions` (or `POST /v2/sessions`)
returns a `session_id`. Everything else below is scoped under it.

### Concepts

**Image refs.** Opening a file registers an *image ref* — a short id like
`img_0`. Derived operations mint their own (`cutout_1`, `bin_1`). One ref per
session is the **active ref**; almost every endpoint accepts an explicit target
via `ref` (alias `image_ref`, and on some routes `image`) and otherwise falls
back to the active ref. With neither, the response is `400 bad_request`
(*"no active image in this session; open a file first"*). Opening, switching
HDU, cutout, and bin each set the new ref active.

**RegionSpec.** Endpoints that operate on a sub-region (`stats`, `histogram`,
`render`, `cutout`) take a `region` object, a tagged union on `type`:

```json
{ "type": "pixel", "x": 0, "y": 0, "width": 512, "height": 512, "clip": false }
```
```json
{ "type": "sky", "ra": 150.113, "dec": 2.205, "size_arcmin": 5.0, "clip": false }
```

- `pixel` — lower-left corner `(x, y)` (0-indexed; `x`=column, `y`=row) and size in pixels.
- `sky` — box centred on ICRS `(ra, dec)` in degrees; `size_arcmin` is either a single number (square) or `[width, height]` in arcmin. Requires a WCS on the image.
- `clip` *(default `false`)* — for `stats`/`histogram`, clamp an over-hanging region to the image instead of erroring `region_out_of_bounds`. `render` always clamps and ignores `clip`; `cutout` NaN-fills off-frame pixels and ignores `clip`.

**Errors.** Same envelope as v1 (`{ "success": false, "error": { "code", "message", "hint"? } }`). v2 adds a few `code`s: `wcs_required` (region/transform needs a WCS the image lacks), `region_out_of_bounds`, `pixel_out_of_bounds`, `not_implemented`.

---

### Sessions & lifecycle

#### `GET /v2/sessions`

List all live sessions (for dashboards). No body. Does **not** refresh any session's idle TTL.

**Response `200`:**
```json
{
  "count": 1,
  "sessions": [
    { "session_id": "a1b2…", "created_unix": 1752460800, "idle_secs": 12,
      "active_ref": "img_0", "image_count": 1, "cache_bytes": 4194304,
      "running_jobs": 0, "last_seq": 7 }
  ]
}
```

#### `GET /v2/sessions/:sid`

Session status snapshot.

**Response `200`:**
```json
{ "session_id": "a1b2…", "active_ref": "img_0", "image_count": 1, "cache_bytes": 4194304 }
```

#### `DELETE /v2/sessions/:sid`

Destroy the session. **Response `204 No Content`**, empty body.

#### `GET /v2/sessions/:sid/history`

Replay the per-session activity ring (capacity 200), oldest-first. Excludes its own polling routes (`history`, `images`, `keepalive`, bare status/DELETE), so polling doesn't pollute the log. Does not refresh idle TTL.

**Query params:** `since_seq` *(u64, default 0)* — only events with `seq` greater than this; `limit` *(usize, optional)* — cap, newest kept.

**Response `200`:**
```json
{
  "session_id": "a1b2…", "first_seq": 51, "last_seq": 60,
  "events": [
    { "seq": 52, "unix_ms": 1752460812345, "method": "POST", "endpoint": "open",
      "image_ref": "img_0", "status": 200, "duration_ms": 8 }
  ]
}
```
`first_seq > since_seq + 1` signals the ring overflowed and events were lost.

#### `POST /v2/sessions/:sid/keepalive`

Refresh the session's idle TTL without doing work. No body.

**Response `200`:** `{ "session_id": "a1b2…", "status": "ok" }`

---

### Images

#### `POST /v2/sessions/:sid/open`

Load a FITS/ASDF file from the server filesystem into a new ref (made active). Never mutates or evicts existing refs.

**Body:** `path` *(string, required)*; `hdu` *(int, optional)* — HDU index, omitted auto-selects the first image HDU (must be omitted for ASDF); `name` *(string, optional)* — explicit ref name, else `img_N`.

**Response `200`:**
```json
{
  "ref": "img_0", "active_ref": "img_0",
  "dims": [2048, 1489], "hdu": 1, "extname": "SCI", "wcs_present": true,
  "stats": { "min": 0.0, "max": 65535.0, "median": 102.5, "mad": 3.1,
             "sigma": 4.6, "mean": 110.2, "valid_count": 3049472 },
  "header": { "SIMPLE": true, "BITPIX": -32, "…": "…" },
  "io": "mmap"
}
```
`dims` is `[cols, rows]` (width, height) — this ordering is used by every v2 response. `hdu` is `null` when auto-selected. `io` is the resolved byte-source policy (`mmap`/`read`/`null`). A bad path/HDU (or an `hdu` passed to ASDF) is `500 internal_error`.

#### `POST /v2/sessions/:sid/hdu`

Load a different HDU from the **active ref's** source file into a new active ref.

**Body:** `hdu` *(int, required)*; `name` *(string, optional)*.

**Response `200`:** same shape as `open` (here `hdu` is always the requested index). `400 bad_request` if there's no active ref or it has no source file.

#### `GET /v2/sessions/:sid/images`

List the session's refs, sorted by id.

**Response `200`:**
```json
{
  "active_ref": "img_1", "count": 2,
  "images": [
    { "image_ref": "img_0", "source": "/data/x.fits", "hdu": 1,
      "width": 2048, "height": 1489, "wcs_present": true, "extname": "SCI" }
  ]
}
```
`source` is `null` for derived refs (cutout/bin).

---

### Inspection

#### `GET /v2/sessions/:sid/structure`

FITS-only HDU listing (reads the source file). **Query:** `ref` (alias `image`).

**Response `200`:**
```json
{
  "ref": "img_1", "count": 2,
  "hdus": [
    { "index": 0, "extname": "PRIMARY", "extver": 1, "naxis": 0, "shape": [],
      "bitpix": 8, "dtype": "uint8", "has_data": false },
    { "index": 1, "extname": "SCI", "extver": 1, "naxis": 2, "shape": [1489, 2048],
      "bitpix": -32, "dtype": "float32", "has_data": true }
  ]
}
```
`shape` is row-major (`[ny, nx]`). `400 bad_request` if the ref has no source file or the source is ASDF.

#### `GET /v2/sessions/:sid/header`

Return header cards. **Query:** `ref` (alias `image`); `keys` *(optional)* — comma-separated keywords or shell globs (`CD*_*`), case-insensitive; omitted returns the full header.

**Response `200`:**
```json
{ "ref": "img_1", "count": 2,
  "cards": { "EXPTIME": { "value": "120.0" }, "FILTER": { "value": "r" } } }
```
`400 bad_request` if the ref carries no header (re-open to attach one).

#### `GET /v2/sessions/:sid/wcs`

WCS summary. **Query:** `ref` (alias `image`).

**Response `200` (WCS present):**
```json
{
  "ref": "img_1", "present": true, "projection": "TAN",
  "crpix": [1024.5, 1024.5], "crval": [150.113, 2.205],
  "cd": [[-1.38e-5, 0.0], [0.0, 1.38e-5]],
  "pixel_scale_arcsec": 0.05, "pixel_scale_x_arcsec": 0.05, "pixel_scale_y_arcsec": 0.05,
  "rotation_deg": 0.0, "flipped": true, "parity": "flipped", "sip_present": false
}
```
When no usable WCS exists this is **not** an error — it returns `{ "ref": "…", "present": false }`.

---

### WCS coordinate transforms

All three accept a target `ref` (alias `image_ref`); `points` are processed in
order. `400 wcs_required` if the image has no usable WCS.

#### `POST /v2/sessions/:sid/wcs/pix2sky`

**Body:** `points` *(array of `[x, y]`, 0-based pixels, required)*; `ref` *(optional)*.

**Response `200`:**
```json
{ "ref": "img_1", "count": 1,
  "results": [ { "x": 100.0, "y": 200.0, "ra": 150.12, "dec": 2.19, "on_image": true } ] }
```

#### `POST /v2/sessions/:sid/wcs/sky2pix`

**Body:** `points` *(array of `[ra, dec]`, ICRS deg, required)*; `ref` *(optional)*.

**Response `200`:**
```json
{ "ref": "img_1", "count": 1,
  "results": [ { "ra": 150.12, "dec": 2.19, "x": 100.3, "y": 200.7, "on_image": true } ] }
```

#### `POST /v2/sessions/:sid/wcs/separation`

Angular separation between two points. Tagged on `type`:

**Body (sky — no WCS needed):** `{ "type": "sky", "a": [ra, dec], "b": [ra, dec] }`
**Body (pixel — via WCS):** `{ "type": "pixel", "a": [x, y], "b": [x, y], "ref": "img_1" }`

**Response `200`:**
```json
{ "separation_deg": 0.1414, "separation_arcmin": 8.485, "separation_arcsec": 509.1 }
```
The `pixel` variant additionally returns `ref`, `a_sky`, `b_sky` (the projected `[ra, dec]`), and `400 region_out_of_bounds` if a point doesn't project onto the sky.

---

### Pixel / stats / histogram

#### `POST /v2/sessions/:sid/pixel`

Value at a pixel plus a neighbourhood summary and sky coords. Read-only.

**Body:** `x`, `y` *(f64, required, 0-based)*; `box` *(int, default `1`)* — neighbourhood side length; `ref` *(optional)*.

**Response `200`:**
```json
{
  "ref": "img_1", "x": 512.0, "y": 480.0, "value": 1203.5, "box": 5,
  "neighborhood": { "min": 10.2, "max": 1203.5, "mean": 245.7, "n_pixels": 24, "n_nan": 1 },
  "sky": { "ra": 150.113, "dec": 2.205 }
}
```
`value` is `null` at a NaN pixel; `sky` is `null` without a WCS. `400 pixel_out_of_bounds` if `(⌊x⌋, ⌊y⌋)` is outside the image.

#### `POST /v2/sessions/:sid/stats`

Region statistics, optional sigma-clip and percentiles.

**Body:** `ref` *(optional)*; `region` *(optional, default full frame)*; `sigma_clip` *(optional object `{ "sigma": 3.0, "maxiters": 5 }`)*; `percentiles` *(optional array, 0–100)*.

**Response `200`:**
```json
{
  "ref": "img_0",
  "region": { "x": 0, "y": 0, "width": 512, "height": 512, "clipped": false },
  "min": 0.12, "max": 30122.0, "median": 210.5, "mad": 12.3, "sigma": 18.2,
  "mean": 215.7, "valid_count": 262100, "n_nan": 44,
  "clipped": { "mean": 214.9, "median": 210.4, "std": 15.1, "n_rejected": 320 },
  "percentiles": [ { "percentile": 99.5, "value": 1024.0 } ]
}
```
`clipped` appears only if `sigma_clip` was given; `percentiles` only if the input array was non-empty.

#### `POST /v2/sessions/:sid/histogram`

**Body:** `ref` *(optional)*; `region` *(optional)*; `bins` *(int, default `256`, must be > 0)*; `range` *(optional `[lo, hi]`, default robust 0.1–99.9th percentile)*; `log_counts` *(bool, default `false`)* — return `ln(1 + count)`.

**Response `200`:**
```json
{
  "ref": "img_0",
  "region": { "x": 0, "y": 0, "width": 512, "height": 512, "clipped": false },
  "bins": [0, 3, 44, 120],
  "bin_edges": [0.1, 1.2, 2.3, 3.4, 4.5],
  "min": 0.1, "max": 1024.0, "log_counts": false, "range_source": "auto", "mode": 210.5
}
```
`bin_edges` has `bins + 1` entries; `range_source` is `"explicit"` or `"auto"`; `mode` is the center of the most-populated bin (`null` if empty).

---

### Derived refs — cutout & bin

Both crop/reduce into a **new active ref** and return the same core body shape
as `open` (`ref`, `active_ref`, `dims`, `stats`, …), plus op-specific fields.

#### `POST /v2/sessions/:sid/cutout`

Crop a region into a new ref. Tolerates partial/zero overlap (off-frame → NaN).

**Body:** `region` *(RegionSpec, required)*; `ref` *(optional source)*; `name` *(optional, else `cutout_N`)*; `preserve_wcs` *(bool, default `true`)* — shift the parent WCS into the cutout header.

**Response `200`** adds `fraction_on_image` (1.0 = fully inside) and `region` (the resolved crop rect in parent pixels; `x`/`y` may be negative). `hdu`/`extname` are `null` for the derived ref.

#### `POST /v2/sessions/:sid/bin`

Block-average rebin by an integer factor; drops WCS.

**Body:** `factor` *(int, required, > 0)* — `out_dims = in_dims / factor` (floor); `method` *(string, default `"mean"`; only `"mean"`)*; `ref` *(optional)*; `name` *(optional, else `bin_N`)*.

**Response `200`** adds `from_ref`, `factor`, `method`; `wcs_present` is always `false`. `400 bad_request` if `factor` exceeds the image dims or `method` isn't `"mean"`.

---

### Render (PNG)

#### `POST /v2/sessions/:sid/render`

The agent-facing render endpoint. Returns a raw **`image/png`** (RGB8). A
response header **`x-render-resolved`** carries a JSON string of the resolved
parameters (vmin/vmax, algorithm, stretch, binning, pixel scale, clipped
fractions). The region always clamps to the image — it never errors
`region_out_of_bounds`.

**Body (all optional except an available ref):**

| Field | Default | Meaning |
|---|---|---|
| `ref` (alias `image`) | active ref | Target image |
| `region` | full frame | RegionSpec; always clamped |
| `scale` | zscale + linear | Scale/stretch config (below) |
| `colormap` | `"gray"` | `gray` or `viridis` |
| `invert_cmap` | `false` | Invert the colormap |
| `max_dim` | none | Long-side cap; bins down by `ceil(max(w,h)/max_dim)` |
| `overlays` | `[]` | `crosshair` (`x,y` or `ra,dec`) and `scalebar` (`length_arcsec`; needs WCS) |

`scale` fields: `algorithm` *(`zscale`\|`minmax`\|`percentile`\|`manual`, default `zscale`)*, `stretch` *(`linear`\|`log`\|`sqrt`\|`asinh`\|`power`, default `linear`)*, `vmin`/`vmax` *(for `manual`)*, `percentile` *(`[lo, hi]`, default `[1.0, 99.5]`)*, `asinh_a` *(default `0.1`)*, `power` *(default `2.0`)*, `zscale_contrast`.

**Body:**
```json
{ "ref": "img_0", "scale": { "algorithm": "zscale", "stretch": "asinh", "asinh_a": 0.1 },
  "colormap": "viridis", "max_dim": 1024 }
```

Unknown `algorithm`/`stretch`/`colormap` → `400 bad_request` (with a hint listing valid values).

---

### Export — compressed FITS

#### `POST /v2/sessions/:sid/export/compressed`

Download a RICE_1-compressed copy of a ref's **source** MEF. Returns
`application/fits` as an attachment (`filename="<ref>_compressed.fits"`).

**Body:** `ref` *(alias `image`/`image_ref`, optional — defaults to active ref)*; `quantize_level` *(f64, default `16.0`)* — noise-relative float quantization; smaller = more aggressive/lossy = smaller file.

**Requires a ref with a source file on disk** — derived refs (cutout/bin) have
none and return `400 bad_request`. `404` if the ref isn't in the session.

```bash
curl -X POST http://localhost:8080/v2/sessions/$SID/export/compressed \
  -H 'Content-Type: application/json' \
  -d '{"ref":"img_0","quantize_level":16.0}' \
  -o img_0_compressed.fits
```

The RICE_1 row encoding is parallelised across cores; bound it with
`RAYON_NUM_THREADS` (see [Configuration](#configuration)).
