/** HTTP client for the b-sandbox API. Same-origin (the console is served by
 * the API itself, or proxied in dev), bearer token from the settings store.
 * Every failure lands in the documented error envelope — the console parses
 * it, never shows a raw stack. */

import type {
  CreateRequest,
  EventsPage,
  ExecOutcome,
  ExecRequest,
  FileEntry,
  Health,
  SandboxDetail,
  SandboxView,
} from "./types";

export class ApiError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
  ) {
    super(`${code}: ${message}`);
  }

  /** True when the configured token is missing or wrong. */
  get isUnauthorized(): boolean {
    return this.code === "unauthorized";
  }
}

/** Token holder: module state + localStorage persistence. One writer (the
 * settings dialog), one reader (every request). */
const TOKEN_KEY = "bs.console.token";

export function loadToken(): string {
  try {
    return window.localStorage.getItem(TOKEN_KEY) ?? "";
  } catch {
    return "";
  }
}

export function saveToken(token: string): void {
  try {
    if (token === "") {
      window.localStorage.removeItem(TOKEN_KEY);
    } else {
      window.localStorage.setItem(TOKEN_KEY, token);
    }
  } catch {
    // Session-only token when storage is unavailable.
  }
}

let token = loadToken();

export function setToken(next: string): void {
  token = next;
  saveToken(next);
}

export function hasToken(): boolean {
  return token !== "";
}

function headers(json: boolean): Record<string, string> {
  const base: Record<string, string> = {};
  if (token !== "") {
    base.Authorization = `Bearer ${token}`;
  }
  if (json) {
    base["Content-Type"] = "application/json";
  }
  return base;
}

async function unwrap<T>(response: Response, parse: (body: string) => T): Promise<T> {
  const body = await response.text();
  if (!response.ok) {
    // The envelope is the contract: {error:{code,message}}.
    try {
      const parsed: unknown = JSON.parse(body);
      const err = (parsed as { error?: { code?: unknown; message?: unknown } }).error;
      if (
        typeof err === "object" &&
        typeof err.code === "string" &&
        typeof err.message === "string"
      ) {
        throw new ApiError(response.status, err.code, err.message);
      }
    } catch (e) {
      if (e instanceof ApiError) {
        throw e;
      }
    }
    throw new ApiError(response.status, "transport", body.slice(0, 300) || response.statusText);
  }
  return parse(body);
}

async function getJson<T>(path: string): Promise<T> {
  const response = await fetch(path, { headers: headers(false) });
  return unwrap(response, (body) => JSON.parse(body) as T);
}

async function postJson<T>(path: string, body: unknown): Promise<T> {
  const response = await fetch(path, {
    method: "POST",
    headers: headers(true),
    body: JSON.stringify(body),
  });
  return unwrap(response, (text) => JSON.parse(text) as T);
}

async function postEmpty(path: string, body: unknown): Promise<void> {
  const response = await fetch(path, {
    method: "POST",
    headers: headers(true),
    body: JSON.stringify(body),
  });
  await unwrap(response, () => undefined);
}

async function request(method: string, path: string, body?: unknown): Promise<void> {
  const response = await fetch(path, {
    method,
    headers: headers(body !== undefined),
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  await unwrap(response, () => undefined);
}

export const api = {
  health: (): Promise<Health> => getJson<Health>("/health"),

  listSandboxes: (): Promise<SandboxView[]> =>
    getJson<{ sandboxes: SandboxView[] }>("/sandboxes").then((r) => r.sandboxes),

  getDetail: (id: string): Promise<SandboxDetail> => getJson<SandboxDetail>(`/sandboxes/${id}`),

  create: (spec: CreateRequest): Promise<SandboxView> => postJson<SandboxView>("/sandboxes", spec),

  terminate: (id: string): Promise<void> => request("DELETE", `/sandboxes/${id}`),

  pause: (id: string): Promise<void> => postEmpty(`/sandboxes/${id}/pause`, {}),

  resume: (id: string): Promise<void> => postEmpty(`/sandboxes/${id}/resume`, {}),

  commitTemplate: (id: string, name: string): Promise<void> =>
    postEmpty(`/sandboxes/${id}/commit`, { name }),

  exec: (id: string, req: ExecRequest): Promise<ExecOutcome> =>
    postJson<ExecOutcome>(`/sandboxes/${id}/exec`, req),

  fileWrite: (id: string, path: string, content: string): Promise<void> =>
    postEmpty(`/sandboxes/${id}/files/write`, { path, content }),

  fileRead: (id: string, path: string): Promise<{ content: string }> =>
    postJson<{ content: string }>(`/sandboxes/${id}/files/read`, { path }),

  fileList: (id: string, path: string): Promise<{ entries: FileEntry[]; truncated: boolean }> =>
    postJson<{ entries: FileEntry[]; truncated: boolean }>(`/sandboxes/${id}/files/list`, {
      path,
    }),

  fileMkdir: (id: string, path: string): Promise<void> =>
    postEmpty(`/sandboxes/${id}/files/mkdir`, { path }),

  fileRemove: (id: string, path: string): Promise<void> =>
    postEmpty(`/sandboxes/${id}/files/remove`, { path }),

  events: (id: string, after: number): Promise<EventsPage> =>
    getJson<EventsPage>(`/sandboxes/${id}/events?after=${after}`),

  metrics: (): Promise<string> =>
    fetch("/metrics", { headers: headers(false) }).then((r) => unwrap(r, (body) => body)),
};
