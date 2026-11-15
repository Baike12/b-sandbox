# b-sandbox Implementation Plan

Status: v0.1 (2026-11-15). Companion to [design.md](./design.md) (Chinese; the architecture contract). This plan is the build order. Files, comments and docs in this repository are English from this point on.

**M1 status: landed.** Modules M-1 through M-8 are implemented and committed (see `git log`); the gate (`scripts/gate.sh`) is green with ~130 tests, including the 18-test Seatbelt enforcement-evidence suite and API black-box adversarial tests. macOS findings that adjusted the plan: `setrlimit(RLIMIT_AS)` fails on macOS so the macOS tier declares no `BUDGET_MEMORY` capability; Seatbelt matches path literals (no kernel re-resolution) so policy and data-plane paths are canonicalized at provision/exec time. Linux (bwrap) integration tests remain gated on a Linux host, as planned.

## 0. Development discipline

Every module follows the same loop, and no module is committed without its gate:

```
implement → unit tests → integration tests (where applicable)
  → scripts/gate.sh (fmt + clippy -D warnings + test --workspace) → commit
```

- One logical change per commit (Conventional Commits, English, imperative).
- Integration tests that need an OS backend run for real on macOS (Seatbelt); Linux-only paths are unit-tested as pure command/policy generation and marked `#[ignore]` for integration with a reason naming the missing privilege.
- Nothing is accepted because "it should work"; every claim in code (a capability bit, a deny rule) has a test that flips the input and observes the behavior change.

## 1. Crate map (M1 scope)

```
crates/
├── sandbox-core/            Pure domain: error, policy, lifecycle, budget, events,
│                            SandboxBackend trait + capability bits. No platform code.
├── sandbox-backend-seatbelt/  macOS Seatbelt (sandbox_init FFI) process backend.
├── sandbox-backend-bwrap/     Linux Bubblewrap process backend (M1: command generation
│                              + policy mapping, unit-tested; integration on Linux CI/server).
└── sandbox-api/             axum REST service: registry, error mapping, file/exec data
                             plane, events endpoint, health. Wires the backends together.
```

Deferred (recorded, not built in M1): `sandbox-sdk` (Rust client), container backend (OCI), microVM backend (libkrun), pause/snapshot, TTL/pause states, SSE streaming, credential-injection egress proxy.

## 2. Milestones (from design.md §13, made concrete)

| Milestone | Ships | Done when |
| --- | --- | --- |
| **M1 minimal loop** | crates above; create → exec → files → kill → events over REST; fs/net/exec policy with default-deny; process isolation via Seatbelt (macOS) / Bubblewrap (Linux); deploy script skeleton aligned with b-superagent | b-superagent can create a sandbox and run a command through the API; the adversarial test suite is green; deployed once on the volcano server (backend availability reported honestly in /health) |
| **M2 production hardening** | Budget ledger (cgroups on Linux), idle TTL + pause state, Prometheus metrics, OpenAPI generation + contract tests, E2B compatibility subset | e2b Python/JS SDK drives the sandbox lifecycle subset; perf gate baselines exist |
| **M3 microVM backend** | libkrun backend + in-guest agent data plane protocol; same API surface | Backend switch is invisible to API clients; escape adversarial suite green on microVM |
| **M4 snapshot & density** | Templates + reflink snapshots + pause/resume consistency pipeline + credential-injection egress proxy | Local benchmark: pause→resume P99 < 1s; test proves credentials never enter the sandbox |

This document details M1 only; later milestones get their own plan docs when M1 lands.

## 3. Module plan (build order, one commit each)

### M-1. Repository scaffold
- Cargo workspace, four crates with smoke tests, `scripts/gate.sh`, `.rustfmt.toml` defaults.
- AGENTS.md test-gate section updated to name `scripts/gate.sh`.
- Done: `scripts/gate.sh` green on an empty workspace.

### M-2. `sandbox-core` error model
- `error.rs`: closed `Error` enum (`Policy`, `Lifecycle`, `Budget`, `Backend`, `Internal`) with stable machine-readable `code()` strings — the single source the API layer maps to HTTP codes. `Internal` never crosses the API boundary unredacted.
- Tests: every variant renders a code; no `unwrap` paths on constructed errors.

### M-3. `sandbox-core` policy model (the heart of M1)
- `policy/`: `fs` (readwrite/readonly/denied sets), `net` (egress allow/deny, CIDR + IP + domain/wildcard), `exec` (rlimits, pids, per-exec timeout bounds).
- Normalization: most-specific-wins for fs paths (deepest wins, MXC semantics); net allow-over-deny with an **unconditional private-range deny list** (RFC1918, loopback, link-local, cloud metadata) that no policy may override.
- Fail-closed validation: unknown/wildcard-hostile inputs are errors, not warnings. No silent clamping of numbers.
- Tests: adversarial set v1 — traversal strings, conflicting path sets, private-range override attempts, malformed CIDR, wildcard domain edge cases (`*`, `*.`, double dot), empty vs absent distinction.

### M-4. `sandbox-core` lifecycle, budget, events
- `lifecycle.rs`: `LifecycleState` state machine (`Provisioning → Running → Terminated`), typed `try_transition`, exec only valid in `Running`. (Paused states arrive in M2; the machine is written so adding them does not rework existing states.)
- `budget.rs`: declared budget (memory MB, pids max, per-exec timeout ms) + enforcement contract per backend; unknown fields rejected by serde.
- `events.rs`: per-sandbox monotonic-seq JSONL event log (`Event { seq, ts, kind, data }`), bounded ring + tail read with `after` cursor — the shape b-superagent already consumes.
- Tests: full transition paths, all illegal transitions rejected, seq monotonicity, `after` cursor semantics, budget rejection on zero/negative.

### M-5. `sandbox-core` backend seam
- `backend.rs`: `SandboxBackend` trait (`provision / exec / stop / deprovision / capabilities / health`) over async, plus `BackendCapabilities` bitmask (fs deny, net egress default, net egress rules, rlimits, pids).
- Rule: a backend refuses at *validate* time anything its capability bits do not declare — `policy_validation` error, never silent degradation.
- Tests: a mock backend proves dispatch enforces the capability contract.

### M-6. Seatbelt backend (macOS, the M1 dev/verification backend)
- Profile generation as a **pure function** policy → Seatbelt scheme (unit-tested): `(deny default)` baseline, read-only system paths allowlist, fs rules from policy, `network-outbound` all-or-nothing per capability bits.
- Enforcement via `sandbox_init` FFI in `pre_exec` (token intentionally kept alive for process lifetime — documented); process tree under `setsid` so kill targets the group.
- Exec: timeout is a result (`TimedOut`), not an error; output capture with a hard byte cap (flood protection), truncation reported.
- Capability declaration: fs rw/ro/deny + net egress default + rlimits; **no CIDR/hostname egress rules** — requesting them is a `policy_validation` error (fail-closed, MXC Seatbelt behavior).
- Integration tests (real macOS): echo round-trip; write outside rootfs denied / inside allowed; TCP to a local listener blocked without egress allow; timeout kills; output flood capped.

### M-7. Bubblewrap backend (Linux)
- Command generation as a **pure function** policy → bwrap argv (ro-binds, `--tmpfs` masks, `--unshare-net`, seccomp filter, rlimits via `--unshare-all` baseline). Unit-tested in argv space.
- Integration tests `#[ignore]` unless `bwrap` present (Linux CI / server); on this macOS dev machine they stay ignored and say so.
- Same capability contract as M-6 minus Seatbelt specifics, plus `net egress rules` capability only when slirp mode is wired (deferred — M1 declares no net-rules capability for bwrap either; keep one honest capability set).

### M-8. `sandbox-api` REST service
- axum server: `POST /sandboxes`, `GET /sandboxes`, `GET/DELETE /sandboxes/{id}`, `POST /sandboxes/{id}/exec`, `POST /sandboxes/{id}/files/{op}` (`read|write|list|stat|remove|mkdir`), `GET /sandboxes/{id}/events?after=`, `GET /health`.
- Registry: in-memory `Arc<RwLock<HashMap>>` of sandbox records; id = prefixed opaque token (`sbx-<random>`).
- File ops: all paths canonicalize inside the sandbox rootfs; traversal is an attack — dedicated adversarial integration tests (`..`, URL-encoded, absolute, symlink escape).
- Error mapping: core `Error.code()` → HTTP status + typed error envelope; unknown backend / missing sandbox / policy_validation each have a locked test.
- Health: reports backend availability and degraded state explicitly; missing backend never falls back to weaker isolation.
- Integration tests: full lifecycle against a real server on a random port with the Seatbelt backend (macOS): create → exec → write/read file → traversal attempts → kill → events seq asserts.

## 4. Known constraints recorded up front

- The volcano-server deployment runs the service inside Docker (b-superagent pipeline). Process backends there run inside that container; `/health` reports exactly what is available. Sibling-container and microVM isolation are M2+/M3 work, respectively — until then the server honestly reports the weaker tier rather than pretending.
- Seatbelt cannot express CIDR/hostname egress rules (MXC reached the same wall); the policy model accepts them only from backends that declare the capability.
- `sandbox_init` FFI is an undocumented-but-stable libSystem symbol; the binding is isolated to one `unsafe` module with the invariant documented.

## 5. Out of scope for M1 (explicitly)

TTL/pause, snapshots, templates beyond `scratch`, SSE, OpenAPI generation, SDK crates, Docker/OCI backend, libkrun, credential injection, multi-node anything.

## 6. M2 module plan (production hardening)

Status: **landed.** Modules P-1 through P-6 are implemented and committed; the gate is green (15 suites, ~170 tests, including 6 contract tests and 2 calibrated perf gates). Measured baselines on the reference dev machine (2026-11-15): seatbelt echo exec 5.2 ms mean (budget 50 ms), API create+exec+detail+delete cycle 0.195 ms (budget 2 ms). serde finding: `#[serde(flatten)]` silently disables `deny_unknown_fields`, so the create body is parsed via explicit envelope extraction + `from_value` into the spec (see routes.rs `parse_create_body`).

Same loop as M1: implement → tests → `scripts/gate.sh` → commit. One commit per module.

| Module | Ships | Done when |
| --- | --- | --- |
| **P-1 deploy skeleton** | `deploy/Dockerfile`, `deploy/compose.yaml`, `scripts/deploy.sh` + `remote_run.sh` + `server_setup.sh` mirroring ../b-superagent (cross-compile → scp → server-side docker build → compose up → curl verify) | `bash scripts/deploy.sh --host …` runs end-to-end on the volcano server; `/health` reports the honest tier inside the container |
| **P-2 budget ledger** | Per-sandbox usage accounting (exec count, output bytes, wall time, created/last-activity) updated at each lifecycle/exec/file touchpoint; exposed in `GET /sandboxes/{id}` | Tests prove every accounting field moves exactly when its cause happens; totals survive a full create→exec→files→delete cycle |
| **P-3 idle TTL + pause** | `timeoutMs` accepted at create (E2B-aligned); lifecycle gains `Paused`; background reaper pauses then terminates idle/expired sandboxes, emitting events | Tests prove an idle sandbox reaches `paused`/`terminated` with the right event sequence and TTL refresh on activity; pause of a Terminated sandbox is a conflict |
| **P-4 Prometheus metrics** | `GET /metrics` in Prometheus text format, hand-rolled (no dependency): sandboxes created/active/terminated, execs (total/timeout/truncated), output bytes | Metric values move in lockstep with the ledger and lifecycle under the same tests |
| **P-5 OpenAPI + contract tests** | `docs/openapi.yaml` (hand-written, versioned) + a contract test that asserts every registered route exists in the spec with matching error codes | Route/spec drift fails the gate |
| **P-6 perf gate** | Startup latency and exec round-trip gate tests, calibrated from measured baselines, order-of-magnitude bounds | Gate tests run in CI without flaking |

Recorded but deferred further: cgroups-v2 memory enforcement for bwrap (needs Linux host evidence; capability bit stays undeclared until a test proves enforcement), e2b SDK driver over the API (envd-protocol bridge belongs with the M3 microVM in-guest agent), SSE streaming, Rust client SDK.
