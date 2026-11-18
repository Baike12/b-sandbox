import { describe, expect, it } from "vitest";
import { parseRoute, routeToHash } from "../logic/router";

describe("parseRoute", () => {
  it("defaults unknown and empty hashes to the guide", () => {
    expect(parseRoute("")).toEqual({ page: "guide" });
    expect(parseRoute("#")).toEqual({ page: "guide" });
    expect(parseRoute("#/nonsense/page")).toEqual({ page: "guide" });
  });

  it("parses the four pages", () => {
    expect(parseRoute("#/guide")).toEqual({ page: "guide" });
    expect(parseRoute("#/create")).toEqual({ page: "create" });
    expect(parseRoute("#/metrics")).toEqual({ page: "metrics" });
    expect(parseRoute("#/sandboxes")).toEqual({ page: "sandboxes", selected: null });
  });

  it("parses sandbox deep links and rejects malformed ids", () => {
    expect(parseRoute("#/sandboxes/sbx-0123456789abcdef")).toEqual({
      page: "sandboxes",
      selected: "sbx-0123456789abcdef",
    });
    expect(parseRoute("#/sandboxes/../../etc/passwd")).toEqual({ page: "guide" });
    expect(parseRoute("#/sandboxes/with space")).toEqual({ page: "guide" });
  });

  it("strips query strings", () => {
    expect(parseRoute("#/metrics?x=1")).toEqual({ page: "metrics" });
  });
});

describe("routeToHash", () => {
  it("round-trips with parseRoute", () => {
    const routes = [
      { page: "guide" as const },
      { page: "create" as const },
      { page: "metrics" as const },
      { page: "sandboxes" as const, selected: null },
      { page: "sandboxes" as const, selected: "sbx-0123456789abcdef" },
    ];
    for (const route of routes) {
      expect(parseRoute(routeToHash(route))).toEqual(route);
    }
  });
});
