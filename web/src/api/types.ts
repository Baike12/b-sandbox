/** Wire types mirroring docs/openapi.yaml v0.4.0. Field-for-field with the
 * serde models; unknown fields never appear (the API rejects them server
 * side, and the console only reads what it declares). */

export interface Health {
  status: "ok" | "degraded";
  version: string;
  backend: {
    name: string;
    available: boolean;
    reason: string | null;
  };
  sandboxes: number;
}

export type EgressDefault = "deny" | "allow";

export interface NetTarget {
  target: string;
}

export interface SandboxSpec {
  fs?: {
    readwrite?: string[];
    readonly?: string[];
    denied?: string[];
  };
  net?: {
    egressDefault?: EgressDefault;
    allow?: NetTarget[];
    deny?: NetTarget[];
    proxy?: boolean;
  };
  exec?: {
    maxOutputBytes?: number;
    maxOpenFiles?: number;
  };
  budget?: {
    memoryMb?: number;
    pidsMax?: number;
  };
  template?: string;
}

export interface SandboxView {
  id: string;
  state: "provisioning" | "running" | "paused" | "terminated";
  rootfs: string;
}

export interface UsageLedger {
  createdAtMs: number;
  lastActivityMs: number;
  execsStarted: number;
  execsFinished: number;
  execsTimedOut: number;
  execWallMs: number;
  outputBytes: number;
  fileOps: number;
}

export interface SandboxDetail extends SandboxView {
  timeoutMs: number | null;
  usage: UsageLedger;
}

export interface ExecRequest {
  command: string;
  args?: string[];
  env?: Record<string, string>;
  cwd?: string;
  timeoutMs?: number;
}

export interface ExecOutcome {
  exitCode: number | null;
  timedOut: boolean;
  stdout: string;
  stderr: string;
  truncated: boolean;
}

export interface FileEntry {
  name: string;
  kind: "file" | "dir" | "symlink" | "other";
  size: number;
}

export interface EventRecord {
  seq: number;
  tsMs: number;
  kind: string;
  data: Record<string, unknown>;
}

export interface EventsPage {
  events: EventRecord[];
  lastSeq: number;
  dropped: number;
}

export interface CreateRequest extends SandboxSpec {
  timeoutMs?: number | null;
}

/** The closed error envelope every failed request answers with. */
export interface ApiErrorEnvelope {
  code: string;
  message: string;
}
