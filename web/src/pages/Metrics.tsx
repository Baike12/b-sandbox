/** Metrics + evaluation. Two halves:
 * 1. 评测标准表 — every service metric with its health threshold and how to
 *    measure it (the standards come from docs/production-readiness.md).
 * 2. 性能自测 — runs 50 real echo execs against a chosen sandbox, computes
 *    p50/p95/mean client-side, and judges against the calibrated budget. */

import { useCallback, useEffect, useMemo, useState } from "react";
import { api, ApiError } from "../api/client";
import type { SandboxView } from "../api/types";
import { Badge, Button, Card, Empty, ErrorBanner, Field } from "../components/bits";
import { gaugeValue, parseMetrics } from "../logic/metrics";
import type { MetricFamily } from "../logic/metrics";
import { judgeLatency, summarizeLatencies } from "../logic/percentiles";
import type { BudgetVerdict } from "../logic/percentiles";

interface Criterion {
  metric: string;
  standard: string;
  how: string;
  /** Live value renderer key, if the metric is scrapeable. */
  live?: "in_flight" | "active" | "created" | "terminated" | "reaped" | "orphans" | "exec_mean";
}

const CRITERIA: readonly Criterion[] = [
  {
    metric: "命令执行延迟（均值 / p50 / p95）",
    standard: "均值 ≤ 50 ms 判定达标（本机基线 ~8 ms，门槛是 ~6x 回归预算）",
    how: "「性能自测」按钮：50 次真实 echo；或 cargo test -p sandbox-backend-seatbelt --test perf",
    live: "exec_mean",
  },
  {
    metric: "bsandbox_http_in_flight",
    standard: "空闲时必须回到 0（不泄漏在途槽位）",
    how: "无请求进行中抓取 /metrics，值应为 0",
    live: "in_flight",
  },
  {
    metric: "bsandbox_sandboxes_active",
    standard: "与 GET /sandboxes 返回数一致（账实相符）",
    how: "对照沙箱列表页计数",
    live: "active",
  },
  {
    metric: "bsandbox_http_requests_total",
    standard: "401/404/413/panic 全部计入（遥测无死角），基数被路由表封顶",
    how: "观察 route 标签只出现路由表内的 pattern；unmatched 只收真正的 404",
  },
  {
    metric: "bsandbox_sandboxes_created_total / terminated_total",
    standard: "terminated = 客户端删除 + reaper + 关停三条路径之和",
    how: "用 terminated − bsandbox_sandboxes_reaped_total 得到客户端删除数",
    live: "terminated",
  },
  {
    metric: "bsandbox_sandboxes_reaped_total",
    standard: "只由空闲 TTL reaper 递增",
    how: "创建一个 1 秒 TTL 的沙箱，等 reaper 扫描（15 秒周期）后观察 +1",
    live: "reaped",
  },
  {
    metric: "bsandbox_orphans_removed_total",
    standard: "每次重启只增一次，等于上次进程遗留的沙箱目录数",
    how: "kill -9 后重启服务，观察启动日志与该计数",
    live: "orphans",
  },
  {
    metric: "服务 RSS",
    standard: "空闲 ≈ 3-5 MB，20 个活跃沙箱 ≈ 5 MB（release 实测）",
    how: "scripts/bench.sh（release 构建 + 真实 Seatbelt 工作负载表）",
  },
  {
    metric: "bsandbox_build_info",
    standard: "总是 1，携带 version 标签（构建元数据）",
    how: "抓取 /metrics 查看 version 标签与 /health 的 version 一致",
  },
];

export function Metrics({ onSelfTestRan }: { onSelfTestRan: () => void }) {
  const [raw, setRaw] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(() => {
    api
      .metrics()
      .then(setRaw)
      .catch((e: unknown) => {
        setError(e instanceof ApiError ? e.message : String(e));
      });
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  const families = useMemo(() => (raw === null ? [] : parseMetrics(raw)), [raw]);

  return (
    <div className="metrics-page">
      <Card
        title="评测标准"
        subtitle="每项指标的健康阈值与测量方法；实时值来自当前 /metrics 抓取。"
        actions={<Button onClick={refresh}>刷新抓取</Button>}
      >
        {error !== null && <ErrorBanner message={error} />}
        <table className="table">
          <thead>
            <tr>
              <th>指标</th>
              <th>评测标准</th>
              <th>怎么测</th>
              <th>实时值</th>
            </tr>
          </thead>
          <tbody>
            {CRITERIA.map((c) => (
              <tr key={c.metric}>
                <td>
                  <b>{c.metric}</b>
                  {c.metric.startsWith("bsandbox") && <br />}
                  {!c.metric.startsWith("bsandbox") && (
                    <span className="muted small">{c.metric}</span>
                  )}
                </td>
                <td>{c.standard}</td>
                <td className="muted small">{c.how}</td>
                <td>
                  {c.live === undefined ? "—" : <LiveValue families={families} which={c.live} />}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </Card>

      <SelfTest onSelfTestRan={onSelfTestRan} />

      {raw !== null && (
        <Card title="原始抓取" subtitle="Prometheus 文本格式 0.0.4，可直接喂给任意抓取器。">
          <details>
            <summary className="muted">展开 /metrics 全文（{raw.split("\n").length} 行）</summary>
            <pre className="stdout">{raw}</pre>
          </details>
        </Card>
      )}
    </div>
  );
}

function LiveValue({
  families,
  which,
}: {
  families: readonly MetricFamily[];
  which: NonNullable<Criterion["live"]>;
}) {
  const num = (n: number | null, unit = ""): string => (n === null ? "无数据" : `${n}${unit}`);
  switch (which) {
    case "in_flight":
      return (
        <Badge tone={gaugeValue(families, "bsandbox_http_in_flight") === 0 ? "ok" : "warn"}>
          {num(gaugeValue(families, "bsandbox_http_in_flight"))}
        </Badge>
      );
    case "active":
      return num(gaugeValue(families, "bsandbox_sandboxes_active"));
    case "created":
      return num(gaugeValue(families, "bsandbox_sandboxes_created_total"));
    case "terminated":
      return num(gaugeValue(families, "bsandbox_sandboxes_terminated_total"));
    case "reaped":
      return num(gaugeValue(families, "bsandbox_sandboxes_reaped_total"));
    case "orphans":
      return num(gaugeValue(families, "bsandbox_orphans_removed_total"));
    case "exec_mean": {
      const count = gaugeValue(families, "bsandbox_exec_duration_seconds_count");
      const sumS = gaugeValue(families, "bsandbox_exec_duration_seconds_sum");
      if (count === null || sumS === null || count === 0) {
        return "尚无执行";
      }
      const meanMs = (sumS / count) * 1000;
      return <Badge tone={meanMs <= 50 ? "ok" : "err"}>{`${meanMs.toFixed(1)} ms`}</Badge>;
    }
  }
}

function SelfTest({ onSelfTestRan }: { onSelfTestRan: () => void }) {
  const [sandboxes, setSandboxes] = useState<SandboxView[] | null>(null);
  const [target, setTarget] = useState<string>("");
  const [progress, setProgress] = useState<{ done: number; total: number } | null>(null);
  const [verdict, setVerdict] = useState<BudgetVerdict | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    api
      .listSandboxes()
      .then((list) => {
        setSandboxes(list);
        const running = list.find((s) => s.state === "running");
        if (running !== undefined) {
          setTarget(running.id);
        }
      })
      .catch((e: unknown) => {
        setError(e instanceof ApiError ? e.message : String(e));
      });
  }, []);

  const RUNS = 50;

  const run = () => {
    if (target === "") {
      setError("先选择或创建一个沙箱");
      return;
    }
    setProgress({ done: 0, total: RUNS });
    setVerdict(null);
    setError(null);

    const samples: number[] = [];
    let failures = 0;
    // Sequential on purpose: the exec gauge and the ledger stay honest, and
    // the wall-time sample per exec is unpolluted by client-side queuing.
    const chain = (i: number): Promise<void> => {
      if (i >= RUNS) {
        const summary = summarizeLatencies(samples, failures);
        setProgress({ done: RUNS, total: RUNS });
        setVerdict(judgeLatency(summary));
        onSelfTestRan();
        return Promise.resolve();
      }
      const started = performance.now();
      return api
        .exec(target, { command: "echo", args: ["ping"], timeoutMs: 5000 })
        .then(() => {
          samples.push(performance.now() - started);
        })
        .catch(() => {
          failures += 1;
        })
        .then(() => {
          setProgress({ done: i + 1, total: RUNS });
          return chain(i + 1);
        });
    };
    void chain(0).catch((e: unknown) => {
      setError(e instanceof ApiError ? e.message : String(e));
      setProgress(null);
    });
  };

  const createOne = () => {
    setError(null);
    api
      .create({ exec: { maxOutputBytes: 65536 } })
      .then((view) => {
        setSandboxes((prev) => (prev === null ? [view] : [...prev, view]));
        setTarget(view.id);
      })
      .catch((e: unknown) => {
        setError(e instanceof ApiError ? e.message : String(e));
      });
  };

  return (
    <Card
      title="性能自测"
      subtitle="对所选沙箱顺序执行 50 次 echo（超时 5 s），客户端计 Wall 时间并判定是否达标。"
    >
      {error !== null && <ErrorBanner message={error} />}
      {sandboxes !== null && sandboxes.length === 0 && <Empty>还没有可用沙箱。</Empty>}
      <div className="inline-row">
        <Field label="目标沙箱">
          <select
            value={target}
            onChange={(e) => {
              setTarget(e.target.value);
            }}
          >
            {sandboxes?.map((s) => (
              <option key={s.id} value={s.id}>
                {s.id}（{s.state}）
              </option>
            ))}
          </select>
        </Field>
        <Button onClick={createOne}>创建临时沙箱</Button>
        <Button variant="primary" onClick={run} disabled={target === "" || progress !== null}>
          {progress === null ? "运行自测（50 次）" : `执行中 ${progress.done}/${progress.total}`}
        </Button>
      </div>
      {progress !== null && (
        <div className="progress-bar">
          <div
            className="progress-fill"
            style={{ width: `${(progress.done / progress.total) * 100}%` }}
          />
        </div>
      )}
      {verdict !== null && (
        <p className="verdict">
          <Badge
            tone={verdict.verdict === "pass" ? "ok" : verdict.verdict === "fail" ? "err" : "warn"}
          >
            {verdict.verdict === "pass"
              ? "达标"
              : verdict.verdict === "fail"
                ? "未达标"
                : "样本不足"}
          </Badge>{" "}
          {verdict.detail}
        </p>
      )}
    </Card>
  );
}
