# M3 — microVM backend design (libkrun)

Status: **designed, not implemented.** Everything in this document is a plan,
not shipped code. See §1 for the one blocker and why it is honest to stop
here; everything else below is the straight line to follow once it clears.

## 1. Why there is no krun crate in this pass

The repo's first principle is *capability declarations need enforcement
evidence*: a backend ships only with tests that flip a policy input and
observe the workload's behavior change on a real machine. libkrun is a Linux
library (KVM + LKVM guests); this development host is macOS, so a
`sandbox-backend-krun` crate written today could not compile against libkrun
headers, could not link, and could not run a single adversarial test. Code
whose isolation has never been observed is exactly the "mostly isolated" the
design doc forbids. The moment a Linux host with `/dev/kvm` exists, §3–§5
turn into work items with their tests already specified.

## 2. Why the backend seam already fits

`SandboxBackend` (provision/exec/stop/deprovision/capabilities/health) was
drawn so container-vs-process-vs-microVM differ only behind the trait. M3
adds one crate and touches nothing else:

```
crates/sandbox-backend-krun/      libkrun microVM backend (Linux only)
├── guest.rs                      in-guest agent: exec/files over vsock
├── vm.rs                         libkrun context lifecycle (create/start/pause)
└── tests/integration.rs          the same adversarial suite, microVM tier
```

- `provision`: build the rootfs image (scratch ext4 or a template from M4's
  snapshot pipeline — reflink templates work unchanged, reflink →
  copy-on-write into the image file), attach via virtiofs/9p.
- `exec`: send the ExecRequest to the in-guest agent (§4); the request shape
  and result (`ExecOutcome`, timeout-as-result, output caps) are already the
  wire contract.
- `capabilities` for the microVM tier: everything the process tier has,
  **plus** `BUDGET_MEMORY` (libkrun enforces guest memory; evidence: a
  memory-hog guest gets OOM-killed by the VM limit) and `NET_EGRESS_RULES`
  (slirp/ passt filtering — evidence per rule like the fs tests).
- `check_policy_support` needs no changes: it already gates on declared bits.

## 3. Isolation model

One guest per sandbox (density arrives with snapshot boot in §6):

| Layer | Mechanism | Evidence test |
| --- | --- | --- |
| Kernel | KVM guest, separate kernel, own PID/IPC/mount/net namespaces | rootfs-escape, fork-bomb, /proc probing suite |
| Budget | libkrun memory ceiling, guest vCPU count | memory-hog OOM within budget |
| FS | guest rootfs image + virtiofs for policy paths; landlock inside the guest mirrors the fs policy | denied-subtree still blocks inside the guest |
| Net | slirp/passt usermode net; egress rules enforced before packets leave the host process | allow-listed host reachable, others refused |
| Credentials | proxy mode maps to a guest-side proxy or slirp gateway binding (decide at implementation; `NET_EGRESS_PROXY` semantics unchanged) | injected header reaches upstream, raw token never visible in guest |

## 4. In-guest agent protocol

The guest runs a small Rust agent (the same `ExecOutcome`/file-op types, so
serde is shared verbatim from sandbox-core):

- Transport: AF_VSOCK stream, port 49982 (E2B's envd port, chosen so tooling
  that speaks envd can also talk to ours), host connects through libkrun's
  vsock device. HTTP/JSON with the same envelope/error codes as the REST API.
- Endpoints: `POST /exec` (ExecRequest → ExecOutcome), `POST /files/{op}`
  (identical shapes to the API data plane), `GET /healthz`.
- Enforcement inside the guest: the agent applies landlock rules derived from
  the resolved fs policy before first exec; guest users are non-root.
- The host API layer proxies agent calls transparently: a client execing
  through b-sandbox cannot tell a process tier from a microVM tier (that is
  the M3 done-criterion: "backend switch is invisible to API clients").

## 5. Acceptance criteria (unchanged from plan.md)

1. The M1 adversarial suite (traversal, symlink escape, flood, fork bomb,
   egress attempts, zombie leakage) passes with the krun backend selected.
2. Memory-budget evidence: a guest exceeding its declared limit dies by OOM,
   and a spec asking for more than the ceiling is rejected.
3. `/health` reports the microVM tier honestly; without `/dev/kvm` the
   backend reports unavailable and the service refuses to start — no silent
   downgrade to the process tier.
4. API contract tests and the SDK live test pass unmodified (no route, shape,
   or error-code change), proving client invisibility.

## 6. Sequenced work items

1. `sandbox-backend-krun` skeleton: health + provision (image from template)
   behind `cfg(target_os = "linux")`, capabilities honestly `NONE` until each
   bit earns its test.
2. Guest agent crate + vsock transport; exec round-trip evidence.
3. Budget + net-rule capability bits with their flip tests.
4. Snapshot boot: fork guests from a paused template VM (COW memory) for
   density; pause/resume consistency pipeline (the M4 pipeline completed).
5. perf gates calibrated like M2's (guest cold boot vs snapshot-boot P99).
