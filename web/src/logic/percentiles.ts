/** Order-statistics for the performance self-check. Nearest-rank percentiles
 * on sorted copies — inputs are never mutated. */

export function percentile(sortedSamples: readonly number[], p: number): number | null {
  if (sortedSamples.length === 0) {
    return null;
  }
  const sorted = [...sortedSamples].sort((a, b) => a - b);
  // Nearest-rank: ceil(p/100 * n), 1-indexed.
  const rank = Math.max(1, Math.ceil((p / 100) * sorted.length));
  return sorted[rank - 1] ?? null;
}

export interface LatencySummary {
  count: number;
  p50Ms: number | null;
  p95Ms: number | null;
  meanMs: number | null;
  failed: number;
}

/** Summarize exec wall times (ms) against the calibrated budget: the repo's
 * gate measures Seatbelt echo at ~8 ms mean; 50 ms is the ~6x regression
 * budget from `sandbox-backend-seatbelt/tests/perf.rs`. */
export function summarizeLatencies(samplesMs: readonly number[], failures: number): LatencySummary {
  const n = samplesMs.length;
  return {
    count: n,
    p50Ms: percentile(samplesMs, 50),
    p95Ms: percentile(samplesMs, 95),
    meanMs: n === 0 ? null : samplesMs.reduce((a, b) => a + b, 0) / n,
    failed: failures,
  };
}

export type BudgetVerdict =
  | { verdict: "pass"; detail: string }
  | { verdict: "fail"; detail: string }
  | { verdict: "insufficient"; detail: string };

const MEAN_BUDGET_MS = 50;
const MIN_SAMPLES = 10;

/** Judge a latency summary against the calibrated budget. Fewer than
 * `MIN_SAMPLES` successful runs is "insufficient" — a verdict must not be
 * minted from noise. */
export function judgeLatency(summary: LatencySummary): BudgetVerdict {
  if (summary.failed > 0) {
    return { verdict: "fail", detail: `${summary.failed} 次执行失败，先排查错误再评测` };
  }
  if (summary.meanMs === null || summary.count < MIN_SAMPLES) {
    return {
      verdict: "insufficient",
      detail: `成功样本 ${summary.count} < ${MIN_SAMPLES}，不足以判定`,
    };
  }
  if (summary.meanMs <= MEAN_BUDGET_MS) {
    return {
      verdict: "pass",
      detail: `均值 ${summary.meanMs.toFixed(1)} ms ≤ 门槛 ${MEAN_BUDGET_MS} ms（p50 ${summary.p50Ms?.toFixed(1)} / p95 ${summary.p95Ms?.toFixed(1)} ms）`,
    };
  }
  return {
    verdict: "fail",
    detail: `均值 ${summary.meanMs.toFixed(1)} ms 超过门槛 ${MEAN_BUDGET_MS} ms（10x 回归预算）`,
  };
}
