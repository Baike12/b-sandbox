import { describe, expect, it } from "vitest";
import { gaugeValue, parseMetrics } from "../logic/metrics";

const EXPOSITION = `# HELP bsandbox_build_info Service build metadata (version label), always 1.
# TYPE bsandbox_build_info gauge
bsandbox_build_info{version="0.4.0"} 1
# HELP bsandbox_http_requests_total HTTP requests by method, route pattern, and status.
# TYPE bsandbox_http_requests_total counter
bsandbox_http_requests_total{method="GET",route="/health",code="200"} 12
bsandbox_http_requests_total{method="POST",route="/sandboxes",code="201"} 3
# HELP bsandbox_sandboxes_active Sandboxes currently in the registry.
# TYPE bsandbox_sandboxes_active gauge
bsandbox_sandboxes_active 2
# TYPE bsandbox_histogram histogram
bsandbox_histogram_bucket{route="/x",le="0.05"} 1
bsandbox_histogram_bucket{route="/x",le="+Inf"} 2
garbage-line-not-a-sample
`;

describe("parseMetrics", () => {
  it("groups families with help, type, and samples", () => {
    const families = parseMetrics(EXPOSITION);
    const build = families.find((f) => f.name === "bsandbox_build_info");
    expect(build?.help).toContain("build metadata");
    expect(build?.type).toBe("gauge");
    expect(build?.samples).toEqual([
      { name: "bsandbox_build_info", labels: { version: "0.4.0" }, value: 1 },
    ]);
  });

  it("parses multi-label samples and multiple series per family", () => {
    const families = parseMetrics(EXPOSITION);
    const requests = families.find((f) => f.name === "bsandbox_http_requests_total");
    expect(requests?.samples).toHaveLength(2);
    expect(requests?.samples[0]?.labels).toEqual({
      method: "GET",
      route: "/health",
      code: "200",
    });
  });

  it("skips malformed lines without dying", () => {
    const families = parseMetrics(EXPOSITION);
    expect(families.find((f) => f.name === "bsandbox_histogram")).toBeDefined();
    expect(families.some((f) => f.name.includes("garbage"))).toBe(false);
  });

  it("reads escaped label values", () => {
    const families = parseMetrics('bsandbox_x{note="a\\"b"} 1\n');
    expect(families[0]?.samples[0]?.labels).toEqual({ note: 'a"b' });
  });
});

describe("gaugeValue", () => {
  it("reads bare values written with empty braces (histogram count/sum)", () => {
    const families = parseMetrics(
      "bsandbox_exec_duration_seconds_count{} 51\nbsandbox_exec_duration_seconds_sum{} 0.33\n",
    );
    expect(gaugeValue(families, "bsandbox_exec_duration_seconds_count")).toBe(51);
    expect(gaugeValue(families, "bsandbox_exec_duration_seconds_sum")).toBeCloseTo(0.33);
  });

  it("returns the bare gauge and null when absent", () => {
    const families = parseMetrics(EXPOSITION);
    expect(gaugeValue(families, "bsandbox_sandboxes_active")).toBe(2);
    expect(gaugeValue(families, "bsandbox_missing")).toBeNull();
  });
});
