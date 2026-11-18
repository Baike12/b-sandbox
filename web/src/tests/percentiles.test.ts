import { describe, expect, it } from "vitest";
import { judgeLatency, percentile, summarizeLatencies } from "../logic/percentiles";

describe("percentile", () => {
  it("computes nearest-rank percentiles without mutating the input", () => {
    const samples = [30, 10, 20, 50, 40];
    expect(percentile(samples, 50)).toBe(30);
    expect(percentile(samples, 95)).toBe(50);
    expect(samples).toEqual([30, 10, 20, 50, 40]);
  });

  it("is null on empty input and total on p=100", () => {
    expect(percentile([], 50)).toBeNull();
    expect(percentile([1, 2], 100)).toBe(2);
  });
});

describe("judgeLatency", () => {
  it("passes under the 50 ms mean budget", () => {
    const summary = summarizeLatencies([8, 9, 10, 8, 7, 9, 8, 10, 9, 8, 12], 0);
    const verdict = judgeLatency(summary);
    expect(verdict.verdict).toBe("pass");
  });

  it("fails over the budget", () => {
    const samples = Array.from({ length: 12 }, () => 80);
    const verdict = judgeLatency(summarizeLatencies(samples, 0));
    expect(verdict.verdict).toBe("fail");
    expect(verdict.detail).toContain("50 ms");
  });

  it("refuses to judge noise or failures", () => {
    expect(judgeLatency(summarizeLatencies([5], 0)).verdict).toBe("insufficient");
    const verdict = judgeLatency(summarizeLatencies([5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5], 2));
    expect(verdict.verdict).toBe("fail");
    expect(verdict.detail).toContain("2 次执行失败");
  });
});
