/** Hash router. The URL is `#/route` — the console is served at `/console`
 * as a single index.html, so no server-side deep links exist. Routes are a
 * discriminated union: an unknown hash parses to `guide` (the onboarding
 * home), never to an unusable blank page. */

export type Route =
  | { page: "guide" }
  | { page: "sandboxes"; selected: string | null }
  | { page: "create" }
  | { page: "metrics" };

export type PageName = Route["page"];

export const NAV: readonly { page: PageName; label: string }[] = [
  { page: "guide", label: "上手指引" },
  { page: "sandboxes", label: "沙箱管理" },
  { page: "create", label: "创建沙箱" },
  { page: "metrics", label: "指标与评测" },
];

/** Parse a location hash into a route. Absent/unknown → guide. */
export function parseRoute(hash: string): Route {
  const path = hash.replace(/^#\/?/, "").split("?")[0];
  if (path === "" || path === "guide") {
    return { page: "guide" };
  }
  if (path === "create") {
    return { page: "create" };
  }
  if (path === "metrics") {
    return { page: "metrics" };
  }
  if (path === "sandboxes") {
    return { page: "sandboxes", selected: null };
  }
  // #/sandboxes/<id> deep link (id charset is restricted server-side).
  const [head, id] = path.split("/");
  if (head === "sandboxes" && id !== "" && /^[A-Za-z0-9_-]{1,64}$/.test(id)) {
    return { page: "sandboxes", selected: id };
  }
  return { page: "guide" };
}

/** Inverse of parseRoute for links and redirects. */
export function routeToHash(route: Route): string {
  switch (route.page) {
    case "guide":
      return "#/guide";
    case "create":
      return "#/create";
    case "metrics":
      return "#/metrics";
    case "sandboxes":
      return route.selected === null ? "#/sandboxes" : `#/sandboxes/${route.selected}`;
  }
}
