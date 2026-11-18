/** Onboarding checklist state machine (pure, vitest-covered). Modeled on the
 * Microsoft Copilot Studio first-run pattern: a persistent get-started
 * checklist where each task auto-completes when the console observes its
 * evidence, with a manual fallback and a progress count.
 *
 * Steps are the four things a first-time user must do: connect (health),
 * create (configure a sandbox), manage (run work in it), evaluate (measure
 * it against the budget). */

export const STEP_IDS = ["connect", "create", "manage", "evaluate"] as const;
export type StepId = (typeof STEP_IDS)[number];

export interface OnboardingState {
  completed: ReadonlySet<StepId>;
  /** Welcome dialog shown exactly once (dismissable). */
  welcomed: boolean;
  /** Spotlight tour shown once; re-openable from the header. */
  tourDone: boolean;
}

export function initialOnboarding(): OnboardingState {
  return { completed: new Set(), welcomed: false, tourDone: false };
}

export type OnboardingEvent =
  | { type: "service_reachable" }
  | { type: "sandbox_created" }
  | { type: "exec_ran" }
  | { type: "self_test_ran" }
  | { type: "mark_step"; step: StepId; done: boolean }
  | { type: "welcome_shown" }
  | { type: "welcome_dismissed" }
  | { type: "tour_finished" };

export function reduce(state: OnboardingState, event: OnboardingEvent): OnboardingState {
  switch (event.type) {
    case "service_reachable":
      return withStep(state, "connect", true);
    case "sandbox_created":
      return withStep(state, "create", true);
    case "exec_ran":
      return withStep(state, "manage", true);
    case "self_test_ran":
      return withStep(state, "evaluate", true);
    case "mark_step":
      return withStep(state, event.step, event.done);
    case "welcome_shown":
      return { ...state, welcomed: true };
    case "welcome_dismissed":
      return { ...state, welcomed: true, tourDone: state.tourDone };
    case "tour_finished":
      return { ...state, welcomed: true, tourDone: true };
  }
}

function withStep(state: OnboardingState, step: StepId, done: boolean): OnboardingState {
  const completed = new Set(state.completed);
  if (done) {
    completed.add(step);
  } else {
    completed.delete(step);
  }
  return { ...state, completed };
}

export function progressCount(state: OnboardingState): number {
  return STEP_IDS.filter((id) => state.completed.has(id)).length;
}

/** Persistence: localStorage, best-effort. A fresh browser restarts the
 * checklist — acceptable for a per-browser guide, never an error path. */
const KEY = "bs.console.onboarding.v1";

export function loadOnboarding(): OnboardingState {
  try {
    const raw = window.localStorage.getItem(KEY);
    if (raw === null) {
      return initialOnboarding();
    }
    const parsed = JSON.parse(raw) as unknown;
    const completed = storedSteps(parsed);
    const flags = storedFlags(parsed);
    if (completed === null || flags === null) {
      return initialOnboarding();
    }
    return {
      completed: new Set(
        completed.filter((s): s is StepId => (STEP_IDS as readonly string[]).includes(s)),
      ),
      welcomed: flags.welcomed,
      tourDone: flags.tourDone,
    };
  } catch {
    return initialOnboarding();
  }
}

/** Shape checks for the stored checklist: unknown JSON never becomes state
 * without passing these. */
function storedSteps(value: unknown): string[] | null {
  if (
    typeof value !== "object" ||
    value === null ||
    !Array.isArray((value as { completed?: unknown }).completed)
  ) {
    return null;
  }
  const list = (value as { completed: unknown[] }).completed;
  return list.every((s) => typeof s === "string") ? list : null;
}

function storedFlags(value: unknown): { welcomed: boolean; tourDone: boolean } | null {
  if (typeof value !== "object" || value === null) {
    return null;
  }
  const welcomed = (value as { welcomed?: unknown }).welcomed;
  const tourDone = (value as { tourDone?: unknown }).tourDone;
  return typeof welcomed === "boolean" && typeof tourDone === "boolean"
    ? { welcomed, tourDone }
    : null;
}

export function saveOnboarding(state: OnboardingState): void {
  try {
    const stored = {
      completed: [...state.completed],
      welcomed: state.welcomed,
      tourDone: state.tourDone,
    };
    window.localStorage.setItem(KEY, JSON.stringify(stored));
  } catch {
    // Private mode / quota: the checklist simply stays session-local.
  }
}
