import { describe, expect, it, vi } from "vitest";
import {
  initialOnboarding,
  loadOnboarding,
  progressCount,
  reduce,
  saveOnboarding,
} from "../logic/onboarding";

describe("onboarding reducer", () => {
  it("auto-completes each step from observed evidence", () => {
    let state = initialOnboarding();
    state = reduce(state, { type: "service_reachable" });
    state = reduce(state, { type: "sandbox_created" });
    state = reduce(state, { type: "exec_ran" });
    expect(progressCount(state)).toBe(3);
    state = reduce(state, { type: "self_test_ran" });
    expect(progressCount(state)).toBe(4);
  });

  it("events are idempotent (re-fetching health never un-does anything)", () => {
    let state = initialOnboarding();
    state = reduce(state, { type: "service_reachable" });
    state = reduce(state, { type: "service_reachable" });
    expect(progressCount(state)).toBe(1);
  });

  it("manual marking can both complete and un-complete a step", () => {
    let state = initialOnboarding();
    state = reduce(state, { type: "mark_step", step: "create", done: true });
    expect(progressCount(state)).toBe(1);
    state = reduce(state, { type: "mark_step", step: "create", done: false });
    expect(progressCount(state)).toBe(0);
  });

  it("welcome and tour are one-shot flags", () => {
    let state = initialOnboarding();
    state = reduce(state, { type: "welcome_dismissed" });
    expect(state.welcomed).toBe(true);
    expect(state.tourDone).toBe(false);
    state = reduce(state, { type: "tour_finished" });
    expect(state.tourDone).toBe(true);
  });
});

describe("persistence", () => {
  it("round-trips through localStorage", () => {
    const store = new Map<string, string>();
    const fakeWindow = {
      localStorage: {
        getItem: (k: string) => store.get(k) ?? null,
        setItem: (k: string, v: string) => store.set(k, v),
        removeItem: (k: string) => store.delete(k),
      },
    };
    vi.stubGlobal("window", fakeWindow);

    let state = initialOnboarding();
    state = reduce(state, { type: "sandbox_created" });
    state = reduce(state, { type: "tour_finished" });
    saveOnboarding(state);

    const loaded = loadOnboarding();
    expect(loaded.completed).toEqual(state.completed);
    expect(loaded.tourDone).toBe(true);

    // Corrupt storage falls back to a fresh checklist, never a crash.
    store.set("bs.console.onboarding.v1", "{not json");
    expect(loadOnboarding().welcomed).toBe(false);

    vi.unstubAllGlobals();
  });
});
