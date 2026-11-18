/** Prometheus text format (0.0.4) parser, scoped to what /metrics emits:
 * `# HELP name text`, `# TYPE name kind`, and `name{labels} value` lines.
 * Parsed into a flat list plus a name-grouped view for the metrics page. */

export interface MetricSample {
  name: string;
  /** Parsed labels, or null for a bare sample. */
  labels: Record<string, string> | null;
  value: number;
}

export interface MetricFamily {
  name: string;
  help: string;
  type: string;
  samples: MetricSample[];
}

/** Parse one exposition document. Malformed value lines are skipped, not
 * fatal — a scrape that grew a new series must still render. */
export function parseMetrics(text: string): MetricFamily[] {
  const help = new Map<string, string>();
  const types = new Map<string, string>();
  const samples = new Map<string, MetricSample[]>();

  for (const rawLine of text.split("\n")) {
    const line = rawLine.trim();
    if (line === "") {
      continue;
    }
    if (line.startsWith("#")) {
      const rest = line.slice(1).trim();
      const space = rest.indexOf(" ");
      if (space < 0) {
        continue;
      }
      const keyword = rest.slice(0, space);
      const body = rest.slice(space + 1);
      const nameSpace = body.indexOf(" ");
      const name = nameSpace < 0 ? body : body.slice(0, nameSpace);
      if (keyword === "HELP" && nameSpace >= 0) {
        help.set(name, body.slice(nameSpace + 1));
      } else if (keyword === "TYPE") {
        types.set(name, nameSpace < 0 ? "untyped" : body.slice(nameSpace + 1));
      }
      continue;
    }
    const parsed = parseSample(line);
    if (parsed !== null) {
      const list = samples.get(parsed.name) ?? [];
      list.push(parsed);
      samples.set(parsed.name, list);
    }
  }

  const names = new Set<string>([...help.keys(), ...types.keys(), ...samples.keys()]);
  return [...names].sort().map((name) => ({
    name,
    help: help.get(name) ?? "",
    type: types.get(name) ?? "untyped",
    samples: samples.get(name) ?? [],
  }));
}

/** `name{a="1",b="2"} 12` or `name 12`. */
function parseSample(line: string): MetricSample | null {
  const brace = line.indexOf("{");
  const space = line.lastIndexOf(" ");
  if (space < 0) {
    return null;
  }
  const rawValue = line.slice(space + 1).trim();
  const value = Number(rawValue);
  if (!Number.isFinite(value)) {
    return null;
  }
  if (brace < 0 || brace > space) {
    return { name: line.slice(0, space).trim(), labels: null, value };
  }
  const name = line.slice(0, brace).trim();
  const labelBody = line.slice(brace + 1, line.lastIndexOf("}"));
  // Empty braces are a bare sample (labels null); malformed non-empty label
  // text drops the line.
  const labels = labelBody.trim() === "" ? null : parseLabels(labelBody);
  if (labels === null && labelBody.trim() !== "") {
    return null;
  }
  return { name, labels, value };
}

function parseLabels(body: string): Record<string, string> | null {
  if (body.trim() === "") {
    // `name{} value` (the server renders histogram series with empty label
    // sets) is a bare sample: labels null, so gaugeValue can find it.
    return null;
  }
  const labels: Record<string, string> = {};
  // Label values are quoted strings; split on commas outside quotes.
  const parts: string[] = [];
  let current = "";
  let inQuotes = false;
  let escaped = false;
  for (const ch of body) {
    if (escaped) {
      current += ch;
      escaped = false;
      continue;
    }
    if (ch === "\\") {
      current += ch;
      escaped = true;
      continue;
    }
    if (ch === '"') {
      inQuotes = !inQuotes;
      current += ch;
      continue;
    }
    if (ch === "," && !inQuotes) {
      parts.push(current);
      current = "";
      continue;
    }
    current += ch;
  }
  if (inQuotes || escaped) {
    return null;
  }
  if (current.trim() !== "") {
    parts.push(current);
  }
  for (const part of parts) {
    const eq = part.indexOf("=");
    if (eq < 0) {
      return null;
    }
    const key = part.slice(0, eq).trim();
    const value = part.slice(eq + 1).trim();
    const unquoted = /^"((?:[^"\\]|\\.)*)"$/.exec(value);
    if (key === "" || unquoted === null) {
      return null;
    }
    const unescaped = unescapePrometheus(unquoted[1]);
    if (unescaped === null) {
      return null;
    }
    labels[key] = unescaped;
  }
  return labels;
}

/** Prometheus label unescaping: `\\` → `\`, `\"` → `"`, `\n` → newline.
 * Not JSON escaping — e.g. `a\"b` is legal exposition but invalid JSON. */
function unescapePrometheus(raw: string): string | null {
  let out = "";
  for (let i = 0; i < raw.length; i++) {
    const ch = raw[i];
    if (ch !== "\\") {
      out += ch;
      continue;
    }
    const next = raw[i + 1];
    if (next === "\\" || next === '"') {
      out += next;
      i++;
      continue;
    }
    if (next === "n") {
      out += "\n";
      i++;
      continue;
    }
    return null;
  }
  return out;
}

/** Pull one gauge-shaped number (no labels) for the evaluation criteria. */
export function gaugeValue(families: readonly MetricFamily[], name: string): number | null {
  const family = families.find((f) => f.name === name);
  const bare = family?.samples.find((s) => s.labels === null);
  return bare === undefined ? null : bare.value;
}
