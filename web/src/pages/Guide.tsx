/** Onboarding home — the get-started center (Microsoft Copilot Studio
 * pattern): a persistent checklist of the four first tasks, each card
 * carrying why / how / an action link, auto-completed from console-observed
 * evidence with a manual fallback. */

import { useState } from "react";
import { Button, Card, Code, ProgressRing } from "../components/bits";
import type { Health } from "../api/types";
import type { OnboardingState, StepId } from "../logic/onboarding";
import { progressCount, reduce } from "../logic/onboarding";
import { STEP_IDS } from "../logic/onboarding";

export function Guide({
  state,
  dispatch,
  health,
  onNavigate,
  onReplayTour,
}: {
  state: OnboardingState;
  dispatch: (event: Parameters<typeof reduce>[1]) => void;
  health: Health | null;
  onNavigate: (page: "create" | "sandboxes" | "metrics") => void;
  onReplayTour: () => void;
}) {
  const [expanded, setExpanded] = useState<StepId | null>(null);
  const done = progressCount(state);

  return (
    <div className="guide">
      <div className="hero">
        <div>
          <h1>欢迎使用 b-sandbox</h1>
          <p className="muted">
            这是给不信任代码准备的隔离执行环境。四步上手：连上服务 → 创建并配置第一个沙箱 →
            在里面跑命令和管理文件 → 用真实指标评测它。每完成一步，下面的清单会自动打勾。
          </p>
          {health !== null && (
            <p className="muted small">
              当前服务：
              <Code>{`v${health.version} · ${health.backend.name} 后端${health.backend.available ? "可用" : "不可用"}`}</Code>
            </p>
          )}
        </div>
        <ProgressRing done={done} total={STEP_IDS.length} />
      </div>

      <div className="step-list">
        {STEPS.map((step) => {
          // Narrow once: the optional actionPage feeds both the guard and the
          // callback (no non-null assertion inside JSX).
          const { actionPage } = step;
          return (
            <StepCard
              key={step.id}
              step={step}
              completed={state.completed.has(step.id)}
              expanded={expanded === step.id}
              onToggle={() => {
                setExpanded(expanded === step.id ? null : step.id);
              }}
              onAction={
                actionPage === undefined
                  ? undefined
                  : () => {
                      onNavigate(actionPage);
                    }
              }
              onMark={(mark) => {
                dispatch({ type: "mark_step", step: step.id, done: mark });
              }}
            />
          );
        })}
      </div>

      <Card title="看完还能再看">
        <div className="tour-actions">
          <Button onClick={onReplayTour}>重新观看页面导览</Button>
          <Button
            variant="ghost"
            onClick={() => {
              for (const id of STEP_IDS) {
                dispatch({ type: "mark_step", step: id, done: false });
              }
            }}
          >
            重置清单进度
          </Button>
        </div>
      </Card>
    </div>
  );
}

interface StepMeta {
  id: StepId;
  title: string;
  summary: string;
  actionLabel?: string;
  actionPage?: "create" | "sandboxes" | "metrics";
  how: string[];
  detail: React.ReactNode;
}

const STEPS: readonly StepMeta[] = [
  {
    id: "connect",
    title: "1 · 连上服务",
    summary: "配置访问令牌，确认 /health 显示后端可用。",
    how: [
      "如果服务以 --auth-token（或环境变量 B_SANDBOX_AUTH_TOKEN）启动，点右上角「设置」粘贴同一个令牌。",
      "顶部徽章变绿（运行中 · 版本 · 后端名）即完成；显示「降级」说明所选隔离后端不可用——服务从不静默退回更弱的隔离。",
    ],
    detail: (
      <ul>
        <li>
          <Code>/health</Code> 是唯一免认证路由，供探活使用；其余全部路由都要 Bearer 令牌。
        </li>
        <li>状态取值只有 ok / degraded：后端缺失时服务宁可报告不可用，也不会假装正常。</li>
        <li>
          令牌只保存在浏览器 localStorage，请求时放在 <Code>Authorization: Bearer …</Code> 头里。
        </li>
      </ul>
    ),
  },
  {
    id: "create",
    title: "2 · 创建并配置沙箱",
    summary: "理解声明式策略（fs/net/exec/budget），提交第一个沙箱。",
    actionLabel: "去创建沙箱 →",
    actionPage: "create",
    how: [
      "「创建沙箱」页有示例预设：最严默认（什么都不授权）→ 常用读写 → 凭据代理模式。",
      "提交成功会回到这里自动打勾；沙箱有 7 天上限的空闲 TTL，没人用会被自动回收。",
    ],
    detail: (
      <ul>
        <li>
          <b>请求体即策略</b>：fs / net / exec / budget 四段声明，未知字段直接报错（fail closed）。
        </li>
        <li>
          <b>fs</b>：readwrite / readonly / denied 三张路径表，全部缺省 =
          工作负载什么都看不到。路径用内核规范拼写（macOS 上是 <Code>/private/var/…</Code> 而不是{" "}
          <Code>/var/…</Code>）。
        </li>
        <li>
          <b>net</b>：egressDefault 默认 <Code>deny</Code>，allow 才开口子；loopback / RFC1918 /
          云元数据 169.254.169.254 等受保护网段永远放不开——写进 allow 也会被拒绝。要注入凭据用{" "}
          <Code>proxy: true</Code>，凭据只存在于代理进程内存。
        </li>
        <li>
          <b>budget</b>：字段<b>写了就是显式要求</b>——后端做不到（比如 macOS
          层的内存上限）就整单拒绝，绝不降级执行。
        </li>
        <li>
          <b>exec</b>：maxOutputBytes 默认 1 MiB（超限截断并标记），maxOpenFiles 默认 256。
        </li>
      </ul>
    ),
  },
  {
    id: "manage",
    title: "3 · 管理沙箱",
    summary: "执行命令、读写文件、看事件流、暂停恢复与终止。",
    actionLabel: "去沙箱管理 →",
    actionPage: "sandboxes",
    how: [
      "选中一个沙箱，在「执行命令」里跑 <Code>echo hello</Code>（或任意命令），成功一次即打勾。",
      "试试文件读写和事件流：每次操作都会以单调序号记进沙箱的事件日志，可按游标重放。",
    ],
    detail: (
      <ul>
        <li>
          exec 是同步调用：返回 exitCode / timedOut / truncated 与捕获的 stdout /
          stderr（超时是结果不是错误）。
        </li>
        <li>终止（DELETE）会杀掉整个进程组——后台化（nohup … &）的子进程也活不过沙箱的删除。</li>
        <li>
          「提交为模板」把当前 rootfs 做成命名模板，下次创建秒级物化（APFS clonefile / FICLONE）。
        </li>
        <li>沙箱终止后事件流确定收口，客户端可带 Last-Event-ID 重连补齐。</li>
      </ul>
    ),
  },
  {
    id: "evaluate",
    title: "4 · 评测性能",
    summary: "读 /metrics 指标，跑一次延迟自测，对照评测标准。",
    actionLabel: "去指标与评测 →",
    actionPage: "metrics",
    how: [
      "在「指标与评测」页点「运行自测」：对真实沙箱执行 50 次 echo，计算 p50 / p95 / 均值。",
      "对照页面里的评测标准表：命令执行均值 ≤ 50 ms 为达标（本机基线约 8 ms，门槛是 10x 回归预算）。",
    ],
    detail: (
      <ul>
        <li>
          agent 场景一次任务跑数百条命令，<b>命令执行延迟是核心指标</b>
          （p50/p95），冷启动只是首因成本。
        </li>
        <li>
          服务侧指标全部来自 <Code>/metrics</Code>（Prometheus
          文本格式）：请求计数/直方图、执行耗时直方图、在途 gauges、reaper 计数。
        </li>
        <li>
          评测标准表给出每项指标的<b>健康阈值与测量方法</b>
          ，全部可在本机复现（scripts/bench.sh、perf 测试）。
        </li>
      </ul>
    ),
  },
];

function StepCard({
  step,
  completed,
  expanded,
  onToggle,
  onAction,
  onMark,
}: {
  step: StepMeta;
  completed: boolean;
  expanded: boolean;
  onToggle: () => void;
  onAction?: () => void;
  onMark: (done: boolean) => void;
}) {
  return (
    <div className={`step-card ${completed ? "done" : ""}`}>
      <button type="button" className="step-head" onClick={onToggle}>
        <span className={`check ${completed ? "checked" : ""}`} aria-hidden>
          {completed ? "✓" : ""}
        </span>
        <span>
          <b>{step.title}</b>
          <span className="muted"> — {step.summary}</span>
        </span>
        <span className="muted small">{expanded ? "收起 ▲" : "展开 ▼"}</span>
      </button>
      {expanded && (
        <div className="step-body">
          <ol className="how">
            {step.how.map((line) => (
              <li key={line}>{line}</li>
            ))}
          </ol>
          <div className="detail">{step.detail}</div>
          <div className="tour-actions">
            {step.actionPage !== undefined && onAction !== undefined && (
              <Button variant="primary" onClick={onAction}>
                {step.actionLabel ?? "开始"}
              </Button>
            )}
            <Button
              variant="ghost"
              onClick={() => {
                onMark(!completed);
              }}
            >
              {completed ? "撤销标记" : "手动标记完成"}
            </Button>
          </div>
        </div>
      )}
    </div>
  );
}
