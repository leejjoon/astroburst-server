# TODOS

## T-1: Per-frame SSE progress (v2) — refactor core/ progress callbacks

**What:** Refactor `Option<&ProgressHandle>` parameters in core/ functions to `Option<&dyn Fn(u32, &str)>`.

**Why:** Enables the headless server to inject `tx.blocking_send()` callbacks into long-running per-frame operations (RL deconvolution, wavelet denoise, drizzle per-frame), so the AI agent sees granular progress events ("drizzling frame 12/45") rather than step-level only.

**Pros:** Per-frame SSE events make 15-min agent runs feel responsive. The `ProgressHandle` refactor also makes `infra/progress.rs` more generic and removes a latent coupling between algorithm code and Tauri's event emitter.

**Cons:** Touches `core/` functions (wavelet_denoise, extract_background, deconvolve_rl), which the Phase 1+2 plan declared off-limits. ~20 function signature changes, minor risk of regression in Tauri handlers that pass a real ProgressHandle.

**Context:** Currently, 3 core functions accept `Option<&ProgressHandle>` (confirmed: `core/imaging/background.rs:58`, `core/imaging/wavelet.rs:37`, `core/stacking/drizzle.rs:266`). In the headless server v1, these are called with `None` (step-level progress only). This TODO makes v2 per-frame progress possible without the `tauri::AppHandle` dependency.

**Depends on:** Phase 1+2 server complete (headless server running, step-level SSE proven).

---

## T-2: reqwest::blocking deadlock guard for Phase 3 SPCC handler

**What:** Document (and enforce via code review) that any server handler invoking `reqwest::blocking::Client` must be wrapped in `tokio::task::spawn_blocking`.

**Why:** `reqwest::blocking` calls inside an async Axum handler (not inside `spawn_blocking`) will deadlock the tokio runtime. SPCC (`cmd/spcc.rs`) currently uses `reqwest::blocking` and is a Phase 3 endpoint. The Phase 3 implementer must know this.

**Pros:** Prevents a runtime panic that won't appear in Phase 1+2 testing and would be subtle to diagnose.

**Cons:** None — this is just a documentation note and a 5-line code pattern.

**Context:** Discovered during Phase 1+2 eng review (outside voice). `reqwest` is in `[dependencies]` with `features = ["blocking"]` (used by `vizier` feature). The server feature will compile this code path. The fix is trivial once you know: wrap SPCC handler body in `tokio::task::spawn_blocking(move || { ... }).await??`.

**Depends on:** Phase 3 SPCC handler implementation.
