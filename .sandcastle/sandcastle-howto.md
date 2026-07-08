# Sandcastle — setup & gotchas how-to

Reusable notes from wiring up [Sandcastle](https://github.com/mattpocock/sandcastle) (`@ai-hero/sandcastle`) on a
real project. `npx @ai-hero/sandcastle init` scaffolds a *generic* template — it assumes a Node project with
`npm run test`/`npm run typecheck` scripts and doesn't build its own sandbox image. Every one of the gotchas below
was a real failure mode hit on a Rust/pnpm project (AstroBurst) that the generic scaffold didn't account for.
Check every item before the first run on a **new** project, even if `init` "succeeded."

---

## One-time repo setup

1. `npx @ai-hero/sandcastle init` → scaffolds `.sandcastle/` (`Containerfile`, `main.ts`, `plan-prompt.md`,
   `implement-prompt.md`, `merge-prompt.md`, `.env.example`, `.gitignore`).
2. Fill `.sandcastle/.env`:
   - `CLAUDE_CODE_OAUTH_TOKEN` (from `claude setup-token`) or `ANTHROPIC_API_KEY`
   - `GH_TOKEN` — a fine-grained PAT with **Issues: Read and write** + **Metadata: Read** on the target repo
3. Confirm the `Sandcastle` label actually exists on the repo (`gh label list`). `init --create-label` is supposed
   to create it, but verify — `plan-prompt.md` filters `gh issue list --label Sandcastle`, so nothing without that
   exact label is ever seen by the planner, no matter what other triage labels it has.
4. Confirm `@ai-hero/sandcastle` is a **real** `devDependency` (`package.json` + `node_modules`), not something
   that only ever got fetched ad hoc via `npx`. `main.ts` does `import * as sandcastle from "@ai-hero/sandcastle"`
   — a static import, which `tsx`/Node cannot auto-resolve the way `npx <pkg>` auto-fetches a CLI. If it's missing,
   add it explicitly (`npm install -D` / `pnpm add -D` / etc., matching your package manager).

---

## Before the first run on any new project — checklist

Each of these bit us once; check all nine before trusting a freshly-scaffolded `.sandcastle/` on a new repo.

1. **Toolchain mismatch.** The scaffolded `Containerfile` is `FROM node:22-bookworm` with only `git`/`curl`/`jq`/
   `gh` installed — nothing else. If the actual work is in a different language (Rust, Go, Python...), the
   container has **no toolchain for it at all**. Add it explicitly:
   - Rust: `rustup` (`curl https://sh.rustup.rs | sh -s -- -y`) + system build deps crates commonly need
     (`build-essential`, `pkg-config`, `libssl-dev`), plus put `~/.cargo/bin` on `PATH`.
   - Same idea for any other language: don't assume the base image has it.

2. **Package manager mismatch.** Check which lockfile is actually committed at the repo root
   (`pnpm-lock.yaml` / `yarn.lock` / `package-lock.json`). The scaffolded `main.ts` hook defaults to
   `{ command: "npm install" }`, which **silently "succeeds" even in a pnpm/yarn project** — it just installs
   into `node_modules` straight from `package.json`, ignoring the real lockfile, which can drift from what's
   actually committed. Match the hook command to the real package manager. Getting pnpm/yarn actually usable
   inside the container takes two things, not just one:
   - `RUN corepack enable` (as root, before the user switch — it writes shims into the global bin dir)
   - `RUN corepack prepare pnpm@<version> --activate` (as the sandbox's non-root user, i.e. **after** the user
     switch) to actually bake the package manager binary into the image at build time.
   Skipping the second step means corepack tries to download the package manager the first time `main.ts`'s
   `onSandboxReady` hook runs `pnpm install` inside the container — which fails outright (`ExecError`,
   "Corepack is about to download...") because newer corepack versions print an interactive download-confirmation
   prompt that has no TTY to answer in a sandbox and just errors instead of proceeding. Also set
   `ENV COREPACK_ENABLE_DOWNLOAD_PROMPT=0` in the `Containerfile` as a second line of defense, in case any
   corepack-mediated fetch happens at a point you didn't anticipate.

3. **Don't "fix" UID/GID by matching the host user — that's backwards.** It's tempting to assume the container's
   `agent` user (`ARG AGENT_UID=1000`/`AGENT_GID=1000` in the scaffolded `Containerfile`) needs to match your real
   host UID/GID (`id -u`/`id -g`) for the bind-mounted worktree to be writable, and to "fix" a permission error by
   rebuilding with `--build-arg AGENT_UID=<host uid>`. **Don't** — for the `podman()` provider specifically
   (`dist/sandboxes/podman.js`), `containerUid`/`containerGid` are **hardcoded to `1000`** unless `main.ts`
   explicitly overrides them, and every sandbox is started with `--user 1000:1000` plus
   `--userns=keep-id:uid=1000,gid=1000` regardless of what the image was built with. `--userns=keep-id` is exactly
   the mechanism that makes a bind-mounted worktree owned by your real host user appear correctly writable to a
   process running as container-UID 1000 — no image rebuild for host-UID matching is needed, or wanted. Rebuilding
   the image with a different `AGENT_UID` only breaks things: the image's *own* internal files (`/home/agent` and
   everything under it, created at build time by `usermod -m`) end up owned by that different UID, while the
   runtime process is still forced to UID 1000 — causing a **new**, different-looking permission failure (e.g.
   `git config --global`'s very first setup step failing with `could not lock config file
   /home/agent/.gitconfig: Permission denied`) that has nothing to do with the original problem.
   **Leave `AGENT_UID`/`AGENT_GID` at their defaults (1000)** unless you deliberately pass matching
   `containerUid`/`containerGid` options to `podman()` in `main.ts` — and if you do, both must agree with each
   other, not with your host UID.

4. **Feedback-loop commands may not exist.** Both `implement-prompt.md` and `merge-prompt.md` hardcode
   `npm run typecheck && npm run test`. Check `package.json`'s `scripts` block for real — plenty of repos have
   neither script, or the actual work under review lives entirely inside a subsystem with its own build tooling
   (a Cargo workspace, a Go module, a nested app). Replace the verification command in **both** prompt files with
   whatever command actually compiles/tests the code the issues touch. If different issues touch different
   subsystems with different verification commands, say so explicitly in each prompt or in the issue body itself.

5. **The sandbox image is never built for you.** Nothing in `main.ts` builds the container image — easy to miss
   since the script otherwise looks self-contained. One-time step before the first `main.ts` run (and again any
   time the `Containerfile` changes):
   ```bash
   npx @ai-hero/sandcastle podman build-image --containerfile .sandcastle/Containerfile
   # or: npx @ai-hero/sandcastle docker build-image --dockerfile .sandcastle/Containerfile
   ```
   Match whichever provider `main.ts` actually uses (`podman()` vs `docker()`). Don't pass `--image-name`
   yourself unless you also pass the matching `imageName` option to the provider in `main.ts` — both the CLI
   build command and the provider factory derive the same default name from the repo path when left unset, so
   leaving both unset is the simplest way to keep them in sync.

6. **Confirm the container runtime actually works** (`podman info` / `docker info`) before touching anything
   else. Rootless podman especially can have host-level setup issues that have nothing to do with Sandcastle and
   are much faster to rule out first.

7. **pnpm's own non-interactive confirmation prompts will abort a sandboxed install — there's no TTY to answer
   them.** Two distinct flavors bit us on the same project:
   - `[ERR_PNPM_IGNORED_BUILDS]` — pnpm 10+ blocks dependency postinstall/build scripts by default. Check that
     `pnpm-workspace.yaml`'s `onlyBuiltDependencies`/`allowBuilds` entries have **real values**, not a half-edited
     placeholder (`esbuild: set this to true or false` is invalid YAML-as-config and silently fails to approve
     anything — it must be a real boolean, e.g. `esbuild: true`). This can be a pre-existing bug in the repo
     itself, unrelated to Sandcastle — worth `git diff`-ing that file before assuming Sandcastle broke something.
   - `[ERR_PNPM_ABORTED_REMOVE_MODULES_DIR_NO_TTY]` — fires when pnpm wants to rebuild an **already-existing**
     `node_modules` (not a fresh install into an empty directory) and can't ask for confirmation. Fix: run the
     hook as `CI=true pnpm install` — this is pnpm's own documented remedy (printed directly in the error text)
     and is the correct default for literally any sandboxed/non-interactive pnpm invocation, not a one-off hack.

8. **`branchStrategy` defaults to "head" mode, which mounts your real repo directory read-write — not an isolated
   worktree.** If a `sandcastle.run()` call doesn't set `branchStrategy` (e.g. the planner phase in the scaffolded
   `main.ts`), Sandcastle skips creating a separate `git worktree` entirely and bind-mounts the actual host
   working directory into the sandbox. This is *why* checklist item 7's second bullet bit us: the planner's
   sandbox saw the real, already-populated `node_modules`, not an empty one. It also means any `hooks.sandbox`
   command in a head-mode run has real read-write access to your actual repo, including uncommitted changes —
   confirm that's what you want (it's reasonable for a read-only planning/analysis agent; less so if you add
   write-side hooks to a head-mode call). Only calls that explicitly pass `branchStrategy: {type: "branch", ...}`
   get true worktree isolation under `.sandcastle/worktrees/`.

9. **Never let a head-mode agent (or its prompt) run `git add -A` / `git add .`.** This is the single most
   destructive thing we hit. Because of item 8, the merger (and any other head-mode call) runs directly against
   your real repo, which almost certainly has real untracked files sitting in it (docs, scratch output, tooling —
   `.sandcastle/` itself is untracked!). A broad `git add -A` stages **all of it**, and a merge-conflict commit
   made on top of that silently bakes those files into history. The real danger isn't the bad commit itself —
   it's what happens if the agent then tries to back out with `git reset --hard <ref>`: that doesn't just unstage,
   it **deletes from disk** anything that was staged/committed but is absent from `<ref>`. An agent that notices
   "oops, I staged too much" and "fixes" it with `reset --hard` will permanently (from git's working-tree
   perspective) wipe every untracked file that got swept up — including `.sandcastle/` itself, mid-run. Fix this
   in `merge-prompt.md` (and any other head-mode prompt) explicitly: *stage only the specific files you resolved
   conflicts in, by name; never `git add -A`/`git add .`; run `git status --porcelain` before committing and abort
   if anything unexpected is staged.*

---

## Recovering from a bad `git reset --hard` (or any accidental history-rewrite)

If an agent's `git reset --hard` (or similar) wipes files you didn't expect, **don't panic — it's very likely
recoverable** as long as the bad state was ever actually committed (even briefly) before being reset away:

1. `git reflog show <branch>` — find the commit reached right *before* the `reset: moving to <ref>` line. That's
   the accidental commit; it still exists as a dangling object even though no branch points to it anymore.
2. `git branch <rescue-name> <that-commit-sha>` — pin it immediately so it can't be garbage-collected while you
   work. This costs nothing and is trivially deletable later.
3. `git show --stat <that-commit-sha>` — confirm it actually contains what you think it does before restoring
   anything.
4. Restore **untracked** files/dirs back to untracked (not re-adding them to history):
   `git checkout <sha> -- <paths...>` (this stages them as a side effect) then
   `git restore --staged <paths...>` (unstages, leaving the working-tree content in place).
5. Restore **tracked files that had uncommitted edits** at the time of the accident the same way — checkout then
   unstage — so they land back as modified-but-uncommitted, matching their state before the accident, not as a
   new commit.
6. Verify nothing legitimate got clobbered in the process (`git status --porcelain`, diff the paths you did *not*
   touch against current `HEAD` to confirm they're untouched, and re-run your build/test command) before deleting
   the rescue branch.

This is exactly how a wiped `.sandcastle/*`, several docs, and uncommitted edits to three tracked files were fully
recovered in practice — nothing was actually lost, it just needed pulling back out of the reflog.

---

## Debugging technique: `ExecError` only shows you `stderr`

When a sandboxed command fails, Sandcastle's `ExecError` message is `Command failed (exit N): <command>\n<stderr>`
— **`stdout` is silently dropped**, even though the underlying `sandbox.exec()` captures both separately. If a
failure shows an exit code with **no further text at all** (as opposed to a real message like the two in item 7),
that's the tell: whatever the command actually complained about went to stdout, not stderr, and you're seeing
nothing. Two ways out, weakest to strongest:

1. **Reproduce manually**, matching the real invocation as closely as possible — `podman run -d --user 1000:1000
   --userns=keep-id:uid=1000,gid=1000 -e HOME=/home/agent -v <dir>:/home/agent/workspace:z -w /home/agent/workspace
   --entrypoint sleep <image> infinity`, then `podman exec <container> sh -c '<the failing command>'` with stdout
   and stderr redirected to separate files so you can see both. Match `<dir>` to what's actually being mounted
   (see item 8 — for a head-mode run, that's your real repo directory, not a fresh worktree; test against a real
   `git worktree add` checkout for a branch-mode run instead).
2. **Or, faster:** temporarily change the failing hook's command to redirect stdout into stderr —
   `"<command> 1>&2"` — so Sandcastle's own error message surfaces everything. This is a one-line, easily-reverted
   edit to `main.ts` and doesn't require standing up a manual reproduction at all; it's usually the fastest path
   to a real error message.

---

## Issue-tracker conventions that make the planner actually work

- Every issue meant for a Sandcastle run needs the `Sandcastle` label — your normal "ready to work" triage label
  (e.g. `ready-for-agent`) is necessary but **not sufficient**; the planner's query only ever sees
  `Sandcastle`-labeled issues.
- Write real `Blocked by #N` lines into issue bodies. `plan-prompt.md`'s dependency-analysis step reads exactly
  this to decide what's safe to parallelize right now vs. what has to wait. A pile of vertical-slice issues that
  all name one shared foundational issue as their blocker works well in practice: iteration 1 does just the
  foundation alone, then every subsequent iteration fans the rest out in parallel once it's merged.

---

## Running it

```bash
npx tsx .sandcastle/main.ts
```

This is a **real, costly, autonomous, long-running** operation, not a quick script: an opus agent plans, N
sonnet/opus agents implement in parallel containers on their own branches, a merge agent folds completed branches
back in and closes issues — repeating up to `MAX_ITERATIONS` times. It makes real commits, real merges, and real
`gh issue close` calls without further confirmation once started. Treat kicking it off like any other action with
a large, hard-to-cheaply-undo blast radius: get everything above verified first, not after.

`result.commits` / `result.branch` / `result.iterations` on the object `sandcastle.run()` resolves to are the
first things to check if a run needs a post-mortem.

---

## Symptoms → root cause (seen in practice)

| Symptom | Cause |
|---|---|
| `Cannot find module '@ai-hero/sandcastle'` | Not installed as a real dependency, only ever resolved ad hoc via `npx` |
| Implementer/merger agent fails immediately at the verification step | `npm run test`/`npm run typecheck` don't exist in `package.json`, or the real work lives in a different subsystem entirely |
| `podman()`/`docker()` sandbox fails to start, or "image not found" | Forgot the one-time `podman build-image`/`docker build-image` step, or changed the `Containerfile` without rebuilding |
| `main.ts` dies on iteration 1 with `ExecError: Command failed (exit 1): pnpm install` / `! Corepack is about to download...` | `corepack enable` alone doesn't bake in the actual package manager binary — add `corepack prepare pnpm@<version> --activate` (as the non-root sandbox user) at build time, and set `COREPACK_ENABLE_DOWNLOAD_PROMPT=0` |
| `git config --global`'s setup step fails with `could not lock config file /home/agent/.gitconfig: Permission denied` | The image was rebuilt with a non-default `AGENT_UID`/`AGENT_GID` (e.g. to "match the host"). Rebuild back to the `Containerfile`'s default (1000:1000 unless `main.ts` explicitly passes different `containerUid`/`containerGid` to `podman()`) — see checklist item 3 |
| `gh issue list --label Sandcastle` returns nothing despite issues existing | Issues carry your triage label but not the literal `Sandcastle` label too |
| Container starts but the agent's own build/test commands fail inside it | `Containerfile` is missing the language toolchain or package manager the project actually needs (see checklist items 1–2) |
| `pnpm install` fails with `[ERR_PNPM_IGNORED_BUILDS]` | `pnpm-workspace.yaml`'s `onlyBuiltDependencies`/`allowBuilds` has an invalid/placeholder value instead of a real boolean — check with `git diff pnpm-workspace.yaml`, this can predate your Sandcastle work entirely |
| `pnpm install` fails with `[ERR_PNPM_ABORTED_REMOVE_MODULES_DIR_NO_TTY]`, or fails with **exit 1 and zero error text** | Run the hook as `CI=true pnpm install` (pnpm needs to rebuild an existing `node_modules` and has no TTY to confirm); if the error text is empty, see "Debugging technique" above — Sandcastle's `ExecError` drops `stdout` entirely |
| A "head mode" sandbox (no `branchStrategy` set) sees stale/unexpected `node_modules`, uncommitted changes, or untracked files | Expected — head mode bind-mounts your real repo directory, not an isolated worktree (checklist item 8) |
| Untracked files (docs, `.sandcastle/*`, scratch output) vanish from disk mid-run, or uncommitted edits to tracked files silently disappear | A head-mode agent ran `git add -A`/`git add .` (sweeping up untracked files) followed by `git reset --hard <ref>` to back out of a mistake — `reset --hard` deletes anything staged/committed but absent from `<ref>`, not just unstages it (checklist item 9). Recoverable — see "Recovering from a bad `git reset --hard`" above |
