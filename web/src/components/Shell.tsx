/** Application shell: header (health badge, token settings, guide entry) and
 * sidebar navigation. Pure chrome — pages render into the content slot. */

import { useState } from "react";
import type { Health } from "../api/types";
import { Badge, Button } from "./bits";
import { NAV } from "../logic/router";
import type { PageName } from "../logic/router";

export function Shell({
  health,
  healthError,
  page,
  onNavigate,
  tokenSet,
  onSaveToken,
  onReplayTour,
  children,
}: {
  health: Health | null;
  healthError: string | null;
  page: PageName;
  onNavigate: (page: PageName) => void;
  tokenSet: boolean;
  onSaveToken: (token: string) => void;
  onReplayTour: () => void;
  children: React.ReactNode;
}) {
  const [tokenDraft, setTokenDraft] = useState("");
  const [settingsOpen, setSettingsOpen] = useState(false);

  return (
    <div className="shell">
      <header className="topbar">
        <div className="brand">
          <span className="brand-mark">b</span>
          <span>b-sandbox 控制台</span>
        </div>
        <div className="topbar-right" data-tour="tour-health">
          {health === null && healthError === null && <Badge tone="muted">连接中…</Badge>}
          {health !== null && health.status === "ok" && (
            <Badge tone="ok">
              运行中 · v{health.version} · {health.backend.name} · {health.sandboxes} 个沙箱
            </Badge>
          )}
          {health !== null && health.status === "degraded" && (
            <Badge tone="warn">
              降级 · {health.backend.name} 不可用
              {health.backend.reason === null ? "" : `（${health.backend.reason}）`}
            </Badge>
          )}
          {healthError !== null && <Badge tone="err">无法连接服务</Badge>}
          <span data-tour="tour-token" className={tokenSet ? "token-dot ok" : "token-dot warn"}>
            {tokenSet ? "令牌已配置" : "未配置令牌"}
          </span>
          <Button
            variant="ghost"
            onClick={() => {
              setSettingsOpen(!settingsOpen);
            }}
          >
            设置
          </Button>
          <Button
            variant="ghost"
            onClick={onReplayTour}
            title="重新观看页面导览"
            data-tour="tour-replay"
          >
            指引
          </Button>
        </div>
      </header>

      {settingsOpen && (
        <div className="settings-pop">
          <label className="field">
            <span className="field-label">访问令牌（Authorization: Bearer）</span>
            <input
              type="password"
              value={tokenDraft}
              placeholder="服务以 --auth-token 启动时必填；留空则清除"
              onChange={(e) => {
                setTokenDraft(e.target.value);
              }}
            />
            <span className="field-hint">
              仅保存在本浏览器 localStorage，从不发送到服务端以外的地址。
            </span>
          </label>
          <div className="tour-actions">
            <Button
              variant="primary"
              onClick={() => {
                onSaveToken(tokenDraft.trim());
                setTokenDraft("");
                setSettingsOpen(false);
              }}
            >
              保存
            </Button>
            <Button
              variant="ghost"
              onClick={() => {
                onSaveToken("");
                setTokenDraft("");
                setSettingsOpen(false);
              }}
            >
              清除
            </Button>
          </div>
        </div>
      )}

      <div className="body">
        <nav className="sidebar" data-tour="tour-nav">
          {NAV.map((item) => (
            <button
              key={item.page}
              type="button"
              className={item.page === page ? "nav-item active" : "nav-item"}
              data-tour={navTourId(item.page)}
              onClick={() => {
                onNavigate(item.page);
              }}
            >
              {item.label}
            </button>
          ))}
          <div className="sidebar-foot">
            <a href="https://github.com/Baike12/b-sandbox" target="_blank" rel="noreferrer">
              文档与源码 ↗
            </a>
          </div>
        </nav>
        <main className="content">{children}</main>
      </div>
    </div>
  );
}

function navTourId(page: PageName): string | undefined {
  switch (page) {
    case "guide":
      return "tour-guide-nav";
    case "create":
      return "tour-create-nav";
    case "sandboxes":
      return "tour-sandboxes-nav";
    case "metrics":
      return "tour-metrics-nav";
  }
}
