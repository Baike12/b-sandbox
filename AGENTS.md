# AGENTS.md — b-sandbox agent conventions

## Project Overview

b-sandbox is a sandbox service for [b-superagent](../b-superagent): it runs untrusted agent workloads — shell commands, generated code, file operations — inside isolated environments and exposes them over an HTTP API. Every workload gets an isolated environment, a declared resource budget, and an explicit network/filesystem policy; nothing an agent submits ever runs bare on the host.

- **Core:** Rust (stable, latest edition, tokio async runtime). This is the center of gravity: isolation backends, workload lifecycle, policy enforcement, and the API service.
- **API surface:** HTTP/JSON REST, E2B-compatible where it costs nothing (the de-facto standard — see `vendor/e2b`, `vendor/cubesandbox`), plus a versioned OpenAPI spec.
- **Frontend (only when a console/viewer is actually needed):** TypeScript (strict) + React + Vite.
- **Deployment target:** the same server as b-superagent, same Docker + compose + nginx-prefix-reverse-proxy pipeline.

## Project Structure

Keep this table honest as code lands. Entry point: `sandbox-api --data-root <dir> [--port 8094] [--backend seatbelt|bwrap]`.

```
crates/                          Rust workspace
├── sandbox-core/                pure domain, no platform code
│   ├── error.rs                 closed error enum + stable codes (API contract)
│   ├── policy/{fs,net,exec}.rs  policy model: fs strictest-wins normalization,
│   │                            protected network ranges, exec bounds
│   ├── lifecycle.rs             state machine (provisioning→running→terminated)
│   ├── budget.rs                memory/pids ceilings (bound-checked, no clamping)
│   ├── events.rs                monotonic-seq bounded event log with after-cursor
│   └── backend.rs               SandboxBackend trait + capability bits +
│                                check_policy_support (reject-don't-degrade)
├── sandbox-backend-seatbelt/    macOS Seatbelt process backend
│   ├── profile.rs               policy → scheme (pure fn, unit-tested)
│   ├── ffi.rs                   sandbox_init seam (returns int; applied in pre_exec)
│   └── tests/integration.rs     enforcement-evidence suite on real processes
├── sandbox-backend-bwrap/       Linux Bubblewrap backend
│   ├── profile.rs               policy → argv (pure fn, unit-tested)
│   └── tests/integration.rs     linux-gated; runs only where bwrap exists
├── sandbox-api/                 axum REST service (lib + thin bin in main.rs)
│   ├── routes.rs                lifecycle/exec/files/events/health handlers
│   ├── files.rs                 sandbox-relative path resolution (traversal +
│   │                            symlink escapes are policy_validation errors)
│   ├── state.rs                 registry (Arc records), id minting, idempotency
│   ├── error.rs                 code→status mapping, ApiJson envelope extractor
│   └── b64.rs                   local base64 (one function, no dependency)
docs/                            design docs and RFCs (design.md 中文, plan.md English)
vendor/                          reference open-source sandboxes — read-only, see vendor/README.md
scripts/gate.sh                  fmt + clippy -D warnings + test --workspace
web/, deploy/, sdk/              not yet (M2+: console, deploy pipeline, client SDK)
```

### Key Concepts

- **One workload = one isolated environment = one budget.** CPU/memory/time/network limits are declared at creation, enforced by the backend, observable for the whole lifetime. No workload exists without them.
- **Policy is data, not flags.** Filesystem/network/exec permissions are declarative policies attached to a workload; enforcement lives in one place per backend, never scattered across call sites.
- **The backend is a seam.** Everything above the backend trait (lifecycle, policy, API, observability) is backend-agnostic: process vs container vs microVM differ only behind the trait. Adding a backend touches nothing else.
- **Fail closed.** Missing policy, unknown backend, failed enforcement, ambiguous path → deny. A sandbox that degrades silently is a security bug, and "mostly isolated" is not isolated. Concretely: `check_policy_support` rejects specs asking for capabilities a backend does not declare; `/health` reports the honest tier; explicit memory budgets are refused on the macOS tier (where they cannot be enforced).
- **Capability declarations need enforcement evidence.** Every bit a backend sets must be backed by a test that flips a policy input and observes the workload's behavior change.
- **Paths are canonical.** fs policy paths and the file data plane operate on kernel-canonical spellings (macOS: /private/var/..., not /var/...); Seatbelt matches the caller's literal string, so both policy and workload must use the same canonical form.

## Code Quality

Architecture and code must read like poetry — simple, clear, elegant — and be fast, lean, and secure by design. (Style contract follows [bnote's AGENTS.md](../bnote/AGENTS.md); the rules below are the load-bearing subset.)

### Rust — five rules

1. **No `unwrap()`, `expect()`, or `panic!` on paths reachable from API input, workload output, or workload files.** Propagate with `?`. Panic only in startup validation of `main`, where a bad config must abort.
2. **Indexing and slicing can panic.** Use `get`, `strip_prefix`, `split_once`; treat workload-supplied paths as adversarial (path traversal is an attack, not just a bug).
3. **Errors cross the API boundary with a typed model.** Internals use `thiserror`; the API layer maps them to stable error codes. A failed request answered with HTTP 200 is a bug.
4. **Never hold a lock across `.await` or blocking work.** Clone what you need, drop the guard, then work. Backend ops (spawn/exec/kill) are async or on dedicated threads, never under a mutex.
5. **One file per logical component; feature directories keep `mod.rs`.** Prefer extending an existing module over adding a near-empty one.

### TypeScript / React (when `web/` exists) — five rules

1. Types at the boundary, no `any`; `strict`, `noUnusedLocals`, `noUnusedParameters` on; `as` needs a one-line reason.
2. Make illegal states unrepresentable — discriminated unions over parallel booleans; derive values instead of storing copies.
3. No floating promises; every rejection reaches the user or is a stated fallback.
4. Pure logic outside UI glue, covered by vitest; components only wire it to the DOM.
5. One owner per state; async results prove they still belong to the current state before writing.

### Design: SOLID and root-cause fixes

- **S** — one module, one model; one piece of state, one writer.
- **O** — extend via the backend trait and policy schema, not by editing call sites.
- **L** — every backend honors the same lifecycle contract; backend quirks stay behind the trait.
- **I** — API endpoints expose exactly the operations callers need; no god-API.
- **D** — core depends on seams (backend trait, policy type, error type); platform code sits in adapters.

**Never patch a bug.** Fix the model, not the symptom: reproduce with evidence → explain the root cause → fix the model (one source of truth, one writer, explicit ordering) → prove the bug class is closed. No special cases, fallbacks, delays, retries, or call-site exceptions.

## Test Gate

Run **`./scripts/gate.sh`** (= `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`) and make it pass before calling any change done. Frontend (if any) adds `pnpm gate` = lint + typecheck + format:check + test.

- **Unit tests** sit next to the module. Lock invariants: lifecycle transitions, budget accounting, policy resolution.
- **Adversarial tests are the product.** A sandbox is only as good as its worst test: path traversal, symlink escapes, fork bombs, output flooding, network egress attempts, zombie leakage, budget overrun. Every rule in the policy model has a test that proves a violation is denied.
- **Integration tests exercise a real backend** (spawn actual workloads in the container/process backend) and are `#[ignore]` only when the host lacks privileges CI can grant.
- **Contract tests lock the API surface**: an OpenAPI diff that breaks a client requires an explicit version bump in the same change.
- **Perf budgets are tests too.** Startup latency and per-request overhead get order-of-magnitude gate tests, calibrated from measurements, never guesswork.

## Deployment

Aligned with [b-superagent](../b-superagent) — same server, same nginx, same pipeline (`scripts/deploy.sh`: cross-compile → scp → `server_setup.sh` → `remote_run.sh` docker build + compose up → curl verify):

- Container binds `127.0.0.1` only, read-only rootfs, `cap_drop: ALL`; extra capabilities each isolation backend needs are granted per-backend and documented in `deploy/`.
- Port plan (server co-tenants): iteach 8080, b-eval 8091, b-superagent 8093 — **b-sandbox takes 8094**; verify against `../b-eval` and iteach before first deploy.
- Secrets never enter images or the repo; config flows through env vars/flags exactly like b-superagent.
- The service degrades observably: if a backend is unavailable at startup, report it in a health endpoint — never silently fall back to a weaker isolation level.

## Vendor Policy

`vendor/` holds reference open-source sandboxes, shallow-cloned from upstream; `vendor/README.md` records upstream URL, pinned commit, and purpose. They are **reading material**: never compiled, linked, or copied into the product without an explicit license check (upstream licenses: Apache-2.0 for cubesandbox/mxc/microsandbox; E2B — check at copy time).

## Git

- Commit on `main` directly (user convention); no feature branches unless the user names one.
- Conventional Commits in English, imperative mood, subject ≤ 20 words: `feat:`, `fix:`, `refactor:`, `test:`, `docs:`, `chore:`. One logical change per commit.
- Commit and push only when the task calls for it; leave reviewable state otherwise.

## Language

- Code, identifiers, comments, doc comments, test names, commit messages: **English**.
- Design docs, README, and product documentation: **Chinese** (aligned with b-superagent's docs).
