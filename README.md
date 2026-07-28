# astroburst-server

A headless HTTP server for FITS/ASDF astronomical image processing, built for AI agents.

`astroburst-server` exposes memory-mapped FITS I/O, WCS coordinate transforms, cutouts, rebinning, region-scoped
statistics/histograms, and rendering (zscale/stretch/colormap) over a REST API — designed for an agent to open a
session, look at an image, reason about what it saw, and iterate, rather than driving a desktop GUI.

## Where this came from

This is a fork of [AstroBurst](https://github.com/samuelkriegerbonini-dev/AstroBurst), a Tauri/WebGPU desktop
FITS viewer, extracted (with commit history intact) down to just the Rust backend and its headless HTTP server.
The desktop/Tauri UI code (`src/cmd/`, `src/main.rs`, `tauri.conf.json`, `gen/`, `capabilities/`, `icons/`) is
still present but **dormant and unmaintained going forward** — this project's direction is server-only. All credit
for the core imaging engine (`src/core/`, `src/infra/`, `src/math/`, `src/types/` — memory-mapped FITS/ASDF I/O,
WCS, stacking, alignment, PSF estimation, deconvolution, and more, proven against real HST/JWST data across ~400
tests) belongs to the original AstroBurst project.

## API design

`astro-image-api.md` is the target design spec for an agent-oriented API — session lifecycle, render-look-adjust
loops, quantitative fallbacks (stats/histogram), resolved-parameter echoes. The current `/v2/*` routes are a
scoped, incrementally-growing subset of that spec; see `SERVER_TESTING.md` for exactly what's implemented today,
with real curl examples and response shapes for every endpoint.

A `v1` route set also exists (`/sessions/*`) — an earlier, more mechanical pass-through of the desktop app's
command vocabulary. It's frozen (not being extended), kept only because existing tooling depends on it.

## Building and running

```bash
cargo build --release \
  --no-default-features \
  --features server,astrometry-net,asdf-full,vizier \
  --bin astroburst-server

./target/release/astroburst-server
```

Binds to `127.0.0.1:8097` by default — see `SERVER.md` for configuration (env vars), the full API reference, and
the session/job model. `SERVER_TESTING.md` has a curl-driven walkthrough of every endpoint.

## Python client

`agent/astroburst_client/` is an async Python SDK for the server (session/job wrappers, SSE streaming). See
`agent/README.md` and `agent/examples/` — including `v2_api_demo.ipynb`, a runnable Jupyter notebook that
exercises every `/v2` endpoint against a real FITS file and is a good first thing to run against a fresh clone.

## Agent-assisted development

This repo uses [Sandcastle](https://github.com/mattpocock/sandcastle) to run GitHub issues through an
autonomous plan → implement → merge loop — see `.sandcastle/sandcastle-howto.md` for the setup and every gotcha
hit getting it working (toolchain/package-manager mismatches, UID/GID handling, the `git add -A` footgun, and
more, written up so the next project doesn't have to rediscover them).

## License

GPL-3.0-or-later, inherited from the original AstroBurst project. See `LICENSE`.
