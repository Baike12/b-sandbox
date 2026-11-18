/** Spec builder. The wire object IS the sandbox spec — the form mirrors the
 * OpenAPI SandboxSpec one-to-one and shows the strictest-default posture
 * next to every field. */

import { useState } from "react";
import { api, ApiError } from "../api/client";
import type { CreateRequest, EgressDefault, SandboxView } from "../api/types";
import { Badge, Button, Card, Code, ErrorBanner, Field } from "../components/bits";

interface FormState {
  template: string;
  timeoutMs: string;
  fsReadwrite: string;
  fsReadonly: string;
  fsDenied: string;
  egressDefault: EgressDefault;
  netAllow: string;
  netDeny: string;
  proxy: boolean;
  maxOutputBytes: string;
  maxOpenFiles: string;
  memoryMb: string;
  pidsMax: string;
}

const INITIAL: FormState = {
  template: "",
  timeoutMs: "",
  fsReadwrite: "",
  fsReadonly: "",
  fsDenied: "",
  egressDefault: "deny",
  netAllow: "",
  netDeny: "",
  proxy: false,
  maxOutputBytes: "",
  maxOpenFiles: "",
  memoryMb: "",
  pidsMax: "",
};

type Preset = "strict" | "readwrite" | "proxy";

const PRESETS: Record<Preset, { label: string; patch: Partial<FormState>; note: string }> = {
  strict: {
    label: "最严默认",
    patch: { ...INITIAL },
    note: "全部字段缺省：文件系统不可见、网络默认拒绝、预算用默认值。适合跑纯计算。",
  },
  readwrite: {
    label: "常用读写",
    patch: {
      ...INITIAL,
      fsReadwrite: "/usr/share",
      fsReadonly: "/etc/ssl",
      netAllow: "example.com\n*.api.example.com",
    },
    note: "可写 /usr/share（示例路径），只读 /etc/ssl，放行 example.com 与其子域。",
  },
  proxy: {
    label: "凭据代理模式",
    patch: { ...INITIAL, proxy: true },
    note: "全部出网走服务的凭据注入代理：沙箱只有一个网络洞 + 标准代理环境变量，凭据永远不进沙箱。",
  },
};

function lines(raw: string): string[] {
  return raw
    .split("\n")
    .map((l) => l.trim())
    .filter((l) => l !== "");
}

function optionalNumber(raw: string): number | undefined {
  if (raw.trim() === "") {
    return undefined;
  }
  const n = Number(raw);
  return Number.isFinite(n) ? n : undefined;
}

function buildSpec(form: FormState): CreateRequest {
  const spec: CreateRequest = {};
  const fs = {
    readwrite: lines(form.fsReadwrite),
    readonly: lines(form.fsReadonly),
    denied: lines(form.fsDenied),
  };
  if (fs.readwrite.length + fs.readonly.length + fs.denied.length > 0) {
    spec.fs = {
      readwrite: fs.readwrite.length > 0 ? fs.readwrite : undefined,
      readonly: fs.readonly.length > 0 ? fs.readonly : undefined,
      denied: fs.denied.length > 0 ? fs.denied : undefined,
    };
  }
  const allow = lines(form.netAllow).map((target) => ({ target }));
  const deny = lines(form.netDeny).map((target) => ({ target }));
  if (allow.length > 0 || deny.length > 0 || form.proxy || form.egressDefault !== "deny") {
    spec.net = {
      egressDefault: form.egressDefault,
      allow: allow.length > 0 ? allow : undefined,
      deny: deny.length > 0 ? deny : undefined,
      proxy: form.proxy,
    };
  }
  const maxOutputBytes = optionalNumber(form.maxOutputBytes);
  const maxOpenFiles = optionalNumber(form.maxOpenFiles);
  if (maxOutputBytes !== undefined || maxOpenFiles !== undefined) {
    spec.exec = { maxOutputBytes, maxOpenFiles };
  }
  const memoryMb = optionalNumber(form.memoryMb);
  const pidsMax = optionalNumber(form.pidsMax);
  if (memoryMb !== undefined || pidsMax !== undefined) {
    spec.budget = { memoryMb, pidsMax };
  }
  if (form.template.trim() !== "") {
    spec.template = form.template.trim();
  }
  const timeoutMs = optionalNumber(form.timeoutMs);
  spec.timeoutMs = timeoutMs === undefined ? null : timeoutMs;
  return spec;
}

export function Create({ onCreated }: { onCreated: (sandbox: SandboxView) => void }) {
  const [form, setForm] = useState<FormState>(INITIAL);
  const [error, setError] = useState<string | null>(null);
  const [created, setCreated] = useState<SandboxView | null>(null);
  const [busy, setBusy] = useState(false);
  const [preset, setPreset] = useState<Preset>("strict");

  const patch = (part: Partial<FormState>) => {
    setForm({ ...form, ...part });
  };

  const submit = () => {
    setBusy(true);
    setError(null);
    api
      .create(buildSpec(form))
      .then((view) => {
        setCreated(view);
        onCreated(view);
      })
      .catch((e: unknown) => {
        setError(e instanceof ApiError ? e.message : String(e));
      })
      .finally(() => {
        setBusy(false);
      });
  };

  return (
    <div className="create">
      <Card
        title="创建沙箱"
        subtitle="策略即请求体：四段声明缺省即为最严格姿态；未知字段会被拒绝。"
        actions={
          <div className="preset-row">
            {(Object.keys(PRESETS) as Preset[]).map((key) => (
              <Button
                key={key}
                variant={preset === key ? "primary" : "default"}
                onClick={() => {
                  setPreset(key);
                  setForm({ ...INITIAL, ...PRESETS[key].patch });
                  setCreated(null);
                }}
              >
                {PRESETS[key].label}
              </Button>
            ))}
          </div>
        }
      >
        <p className="muted small">{PRESETS[preset].note}</p>

        {error !== null && <ErrorBanner message={error} />}
        {created !== null && (
          <div className="ok-banner">
            <Badge tone="ok">创建成功</Badge> <Code>{created.id}</Code>{" "}
            <span className="muted">状态 {created.state}</span>
            <Button
              variant="ghost"
              onClick={() => {
                window.location.hash = `#/sandboxes/${created.id}`;
              }}
            >
              去管理它 →
            </Button>
          </div>
        )}

        <div className="form-grid">
          <Field label="模板（可选）" hint="从已提交的命名模板物化 rootfs（秒级 clonefile）。">
            <input
              value={form.template}
              onChange={(e) => {
                patch({ template: e.target.value });
              }}
              placeholder="例如 my-template"
            />
          </Field>
          <Field
            label="空闲 TTL ms（可选）"
            hint="超过该时长无数据面活动会被 reaper 终止；留空 = 不设 TTL。"
          >
            <input
              value={form.timeoutMs}
              onChange={(e) => {
                patch({ timeoutMs: e.target.value });
              }}
              placeholder="如 3600000（1 小时）"
              inputMode="numeric"
            />
          </Field>
        </div>

        <div className="form-grid">
          <Field
            label="fs.readwrite（每行一个路径）"
            hint="可写授权。macOS 后端需要内核规范拼写：/private/var/… 而不是 /var/…。"
          >
            <textarea
              rows={3}
              value={form.fsReadwrite}
              onChange={(e) => {
                patch({ fsReadwrite: e.target.value });
              }}
              placeholder={"/usr/share"}
            />
          </Field>
          <Field label="fs.readonly（每行一个路径）" hint="只读挂载，工作负载可读不可写。">
            <textarea
              rows={3}
              value={form.fsReadonly}
              onChange={(e) => {
                patch({ fsReadonly: e.target.value });
              }}
              placeholder={"/etc/ssl"}
            />
          </Field>
          <Field
            label="fs.denied（每行一个路径）"
            hint="即使父目录已授权也强制屏蔽（子树整体消失）。"
          >
            <textarea
              rows={2}
              value={form.fsDenied}
              onChange={(e) => {
                patch({ fsDenied: e.target.value });
              }}
              placeholder={"/private/etc"}
            />
          </Field>
        </div>

        <div className="form-grid">
          <Field label="net.egressDefault" hint="默认 deny：未在 allow 中列出的目标一律拒绝。">
            <select
              value={form.egressDefault}
              onChange={(e) => {
                patch({ egressDefault: e.target.value === "allow" ? "allow" : "deny" });
              }}
            >
              <option value="deny">deny（推荐）</option>
              <option value="allow">allow</option>
            </select>
          </Field>
          <Field
            label="net.allow / deny（每行一个目标）"
            hint="CIDR（10.0.0.0/8）或域名（example.com、*.example.com）。loopback/RFC1918/169.254/169.254 等受保护网段写了也会被拒绝。"
          >
            <textarea
              rows={2}
              value={form.netAllow}
              onChange={(e) => {
                patch({ netAllow: e.target.value });
              }}
              placeholder={"example.com\n*.api.example.com"}
            />
            <textarea
              rows={2}
              value={form.netDeny}
              onChange={(e) => {
                patch({ netDeny: e.target.value });
              }}
              placeholder={"203.0.113.0/24"}
            />
          </Field>
          <Field
            label="net.proxy"
            hint="出网全部走凭据注入代理（需要 seatbelt 层 + 部署时配置凭据文件）；开启后 allow/deny 与 allow 默认姿态无效。"
          >
            <input
              type="checkbox"
              checked={form.proxy}
              onChange={(e) => {
                patch({ proxy: e.target.checked });
              }}
            />
          </Field>
        </div>

        <div className="form-grid">
          <Field
            label="exec.maxOutputBytes（可选）"
            hint="默认 1 MiB；超出即截断并标记 truncated。"
          >
            <input
              value={form.maxOutputBytes}
              onChange={(e) => {
                patch({ maxOutputBytes: e.target.value });
              }}
              placeholder="1048576"
              inputMode="numeric"
            />
          </Field>
          <Field
            label="exec.maxOpenFiles（可选）"
            hint="默认 256，以硬上限写入（工作负载无法自行抬回）。"
          >
            <input
              value={form.maxOpenFiles}
              onChange={(e) => {
                patch({ maxOpenFiles: e.target.value });
              }}
              placeholder="256"
              inputMode="numeric"
            />
          </Field>
          <Field
            label="budget.memoryMb（可选）"
            hint="存在即意图：显式写出（哪怕是默认值 2048）就要求后端能执行该上限，否则整单拒绝。macOS 层不支持，留空用默认。"
          >
            <input
              value={form.memoryMb}
              onChange={(e) => {
                patch({ memoryMb: e.target.value });
              }}
              placeholder="2048"
              inputMode="numeric"
            />
          </Field>
          <Field label="budget.pidsMax（可选）" hint="进程数上限，默认 512；fork bomb 由它拦下。">
            <input
              value={form.pidsMax}
              onChange={(e) => {
                patch({ pidsMax: e.target.value });
              }}
              placeholder="512"
              inputMode="numeric"
            />
          </Field>
        </div>

        <div className="tour-actions">
          <Button variant="primary" onClick={submit} disabled={busy}>
            {busy ? "创建中…" : "创建沙箱"}
          </Button>
          <span className="muted small">
            等价请求：<Code>POST /sandboxes</Code>（请求体即上面的策略）
          </span>
        </div>
      </Card>
    </div>
  );
}
