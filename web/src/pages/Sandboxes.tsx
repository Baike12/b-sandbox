/** Sandbox management: list, detail (usage ledger, events), exec, file ops,
 * lifecycle actions. SSE with a Bearer header is not possible from a browser
 * (EventSource cannot set headers), so events poll the REST cursor instead. */

import { useCallback, useEffect, useState } from "react";
import { api, ApiError } from "../api/client";
import type { EventRecord, ExecOutcome, SandboxDetail, SandboxView } from "../api/types";
import { Badge, Button, Card, Code, Empty, ErrorBanner, Field } from "../components/bits";
import type { Tone } from "../components/bits";

function stateTone(state: SandboxView["state"]): Tone {
  switch (state) {
    case "running":
      return "ok";
    case "provisioning":
      return "info";
    case "paused":
      return "warn";
    case "terminated":
      return "muted";
  }
}

export function Sandboxes({
  selected,
  onExecRan,
}: {
  selected: string | null;
  onExecRan: () => void;
}) {
  const [list, setList] = useState<SandboxView[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(() => {
    api
      .listSandboxes()
      .then(setList)
      .catch((e: unknown) => {
        setError(e instanceof ApiError ? e.message : String(e));
      });
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  return (
    <div className="sandboxes">
      <Card
        title="沙箱列表"
        actions={
          <Button onClick={refresh}>{list === null ? "刷新" : `刷新（${list.length}）`}</Button>
        }
      >
        {error !== null && <ErrorBanner message={error} />}
        {list !== null && list.length === 0 && (
          <Empty>还没有沙箱。去「创建沙箱」页用示例预设创建第一个。</Empty>
        )}
        {list !== null && list.length > 0 && (
          <table className="table">
            <thead>
              <tr>
                <th>id</th>
                <th>状态</th>
                <th>rootfs</th>
              </tr>
            </thead>
            <tbody>
              {list.map((s) => (
                <SandboxRow key={s.id} view={s} selected={s.id === selected} />
              ))}
            </tbody>
          </table>
        )}
      </Card>

      {selected !== null && <Detail id={selected} onExecRan={onExecRan} onChanged={refresh} />}
    </div>
  );
}

function SandboxRow({ view, selected }: { view: SandboxView; selected: boolean }) {
  return (
    <tr className={selected ? "row-selected" : ""}>
      <td>
        <a href={`#/sandboxes/${view.id}`}>
          <Code>{view.id}</Code>
        </a>
      </td>
      <td>
        <Badge tone={stateTone(view.state)}>{view.state}</Badge>
      </td>
      <td>
        <code className="code small">{view.rootfs}</code>
      </td>
    </tr>
  );
}

function Detail({
  id,
  onExecRan,
  onChanged,
}: {
  id: string;
  onExecRan: () => void;
  onChanged: () => void;
}) {
  const [detail, setDetail] = useState<SandboxDetail | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [events, setEvents] = useState<EventRecord[]>([]);
  const [cursor, setCursor] = useState(0);
  const [dropped, setDropped] = useState(0);

  const loadDetail = useCallback(() => {
    api
      .getDetail(id)
      .then(setDetail)
      .catch((e: unknown) => {
        setError(e instanceof ApiError ? e.message : String(e));
      });
  }, [id]);

  useEffect(() => {
    setDetail(null);
    setError(null);
    setEvents([]);
    setCursor(0);
    setDropped(0);
    loadDetail();
  }, [id, loadDetail]);

  // Poll detail + events while the drawer is open. REST polling, not SSE:
  // browsers cannot attach an Authorization header to EventSource.
  useEffect(() => {
    const timer = window.setInterval(() => {
      loadDetail();
      api
        .events(id, cursor)
        .then((page) => {
          if (page.events.length > 0) {
            setEvents((prev) => [...prev, ...page.events].slice(-200));
            setCursor(page.lastSeq);
          }
          setDropped(page.dropped);
        })
        .catch(() => undefined);
    }, 2000);
    return () => {
      window.clearInterval(timer);
    };
  }, [id, cursor, loadDetail]);

  const run = (action: () => Promise<unknown>, after?: () => void) => {
    setError(null);
    action()
      .then(() => {
        after?.();
        loadDetail();
        onChanged();
      })
      .catch((e: unknown) => {
        setError(e instanceof ApiError ? e.message : String(e));
      });
  };

  if (error !== null && detail === null) {
    return <ErrorBanner message={error} />;
  }
  if (detail === null) {
    return <Empty>加载中…</Empty>;
  }

  const usage = detail.usage;
  return (
    <>
      <Card
        title={
          <span>
            <Code>{detail.id}</Code> <Badge tone={stateTone(detail.state)}>{detail.state}</Badge>
          </span>
        }
        subtitle={`rootfs ${detail.rootfs} · 空闲 TTL ${detail.timeoutMs === null ? "未设置" : `${detail.timeoutMs} ms`}`}
        actions={
          <>
            <Button
              onClick={() => {
                run(() => api.pause(id));
              }}
              disabled={detail.state !== "running"}
            >
              暂停
            </Button>
            <Button
              onClick={() => {
                run(() => api.resume(id));
              }}
              disabled={detail.state !== "paused"}
            >
              恢复
            </Button>
            <Button
              variant="danger"
              onClick={() => {
                run(
                  () => api.terminate(id),
                  () => {
                    setDetail(null);
                  },
                );
              }}
            >
              终止
            </Button>
          </>
        }
      >
        {error !== null && <ErrorBanner message={error} />}
        <div className="ledger-grid">
          <LedgerCell
            label="exec 次数"
            value={`${usage.execsStarted} 起 / ${usage.execsFinished} 完 / ${usage.execsTimedOut} 超时`}
          />
          <LedgerCell label="累计执行时长" value={`${usage.execWallMs} ms`} />
          <LedgerCell label="输出字节" value={`${usage.outputBytes}`} />
          <LedgerCell label="文件操作" value={`${usage.fileOps}`} />
          <LedgerCell label="创建时间" value={new Date(usage.createdAtMs).toLocaleString()} />
          <LedgerCell label="最近活动" value={new Date(usage.lastActivityMs).toLocaleString()} />
        </div>
        <ExecPanel id={id} onExecRan={onExecRan} onDone={loadDetail} onError={setError} />
        <TemplatePanel id={id} onError={setError} onDone={loadDetail} />
      </Card>

      <Card
        title="事件日志"
        subtitle={`游标 ${cursor} · 丢弃 ${dropped}（2 秒轮询，单调序号可重放）`}
      >
        {events.length === 0 && <Empty>暂无事件。</Empty>}
        {events.length > 0 && (
          <table className="table">
            <thead>
              <tr>
                <th>seq</th>
                <th>时间</th>
                <th>kind</th>
                <th>data</th>
              </tr>
            </thead>
            <tbody>
              {events.map((e) => (
                <tr key={e.seq}>
                  <td>{e.seq}</td>
                  <td>{new Date(e.tsMs).toLocaleTimeString()}</td>
                  <td>
                    <Badge tone="info">{e.kind}</Badge>
                  </td>
                  <td>
                    <code className="code small">{JSON.stringify(e.data)}</code>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </Card>

      <FilePanel id={id} onDone={loadDetail} onError={setError} />
    </>
  );
}

function LedgerCell({ label, value }: { label: string; value: string }) {
  return (
    <div className="ledger-cell">
      <span className="muted small">{label}</span>
      <b>{value}</b>
    </div>
  );
}

function ExecPanel({
  id,
  onExecRan,
  onDone,
  onError,
}: {
  id: string;
  onExecRan: () => void;
  onDone: () => void;
  onError: (message: string) => void;
}) {
  const [command, setCommand] = useState("echo");
  const [argsRaw, setArgsRaw] = useState("hello");
  const [timeoutMs, setTimeoutMs] = useState("");
  const [outcome, setOutcome] = useState<ExecOutcome | null>(null);
  const [busy, setBusy] = useState(false);

  const run = () => {
    setBusy(true);
    const args = argsRaw.split(/\s+/).filter((a) => a !== "");
    const parsedTimeout = timeoutMs.trim() === "" ? undefined : Number(timeoutMs);
    api
      .exec(id, {
        command,
        args,
        timeoutMs: Number.isFinite(parsedTimeout) ? parsedTimeout : undefined,
      })
      .then((result) => {
        setOutcome(result);
        onExecRan();
        onDone();
      })
      .catch((e: unknown) => {
        onError(e instanceof ApiError ? e.message : String(e));
      })
      .finally(() => {
        setBusy(false);
      });
  };

  return (
    <div className="exec-panel">
      <div className="form-grid">
        <Field label="命令">
          <input
            value={command}
            onChange={(e) => {
              setCommand(e.target.value);
            }}
            placeholder="/bin/echo"
          />
        </Field>
        <Field label="参数（空格分隔）">
          <input
            value={argsRaw}
            onChange={(e) => {
              setArgsRaw(e.target.value);
            }}
            placeholder="hello world"
          />
        </Field>
        <Field label="超时 ms（可选，上限 600000）">
          <input
            value={timeoutMs}
            onChange={(e) => {
              setTimeoutMs(e.target.value);
            }}
            placeholder="5000"
            inputMode="numeric"
          />
        </Field>
        <div className="field">
          <span className="field-label">&nbsp;</span>
          <Button variant="primary" onClick={run} disabled={busy || command.trim() === ""}>
            {busy ? "执行中…" : "执行"}
          </Button>
        </div>
      </div>
      {outcome !== null && (
        <div className="outcome">
          <p>
            <Badge tone={outcome.exitCode === 0 && !outcome.timedOut ? "ok" : "err"}>
              {outcome.timedOut
                ? "超时"
                : `退出码 ${outcome.exitCode === null ? "信号终止" : outcome.exitCode}`}
            </Badge>
            {outcome.truncated && <Badge tone="warn">输出被截断</Badge>}
          </p>
          {outcome.stdout !== "" && <pre className="stdout">{outcome.stdout}</pre>}
          {outcome.stderr !== "" && <pre className="stderr">{outcome.stderr}</pre>}
        </div>
      )}
    </div>
  );
}

function TemplatePanel({
  id,
  onDone,
  onError,
}: {
  id: string;
  onDone: () => void;
  onError: (message: string) => void;
}) {
  const [name, setName] = useState("");
  return (
    <div className="template-row">
      <Field label="提交为模板（把当前 rootfs 快照成命名模板，下次创建直接物化）">
        <div className="inline-row">
          <input
            value={name}
            onChange={(e) => {
              setName(e.target.value);
            }}
            placeholder="模板名：小写字母/数字/连字符"
          />
          <Button
            disabled={!/^[a-z0-9][a-z0-9-]{0,62}$/.test(name)}
            onClick={() => {
              onError("");
              api
                .commitTemplate(id, name)
                .then(onDone)
                .catch((e: unknown) => {
                  onError(e instanceof ApiError ? e.message : String(e));
                });
            }}
          >
            提交模板
          </Button>
        </div>
      </Field>
    </div>
  );
}

function FilePanel({
  id,
  onDone,
  onError,
}: {
  id: string;
  onDone: () => void;
  onError: (message: string) => void;
}) {
  const [path, setPath] = useState("hello.txt");
  const [content, setContent] = useState("你好，沙箱\n");
  const [listing, setListing] = useState<string | null>(null);
  const [readBack, setReadBack] = useState<string | null>(null);

  const fail = (e: unknown) => {
    onError(e instanceof ApiError ? e.message : String(e));
  };

  return (
    <Card
      title="文件操作"
      subtitle="路径是沙箱相对路径；不允许 ../、symlink 逃逸与悬空 symlink 尾部（写穿透攻击在策略层被拒）。"
    >
      <div className="inline-row">
        <input
          value={path}
          onChange={(e) => {
            setPath(e.target.value);
          }}
          placeholder="相对路径"
        />
        <Button
          onClick={() => {
            onError("");
            api.fileWrite(id, path, content).then(onDone).catch(fail);
          }}
        >
          写入
        </Button>
        <Button
          onClick={() => {
            onError("");
            api
              .fileRead(id, path)
              .then((r) => {
                setReadBack(r.content);
              })
              .catch(fail);
          }}
        >
          读取
        </Button>
        <Button
          onClick={() => {
            onError("");
            api
              .fileList(id, ".")
              .then((r) => {
                setListing(
                  r.entries.map((e) => `${e.kind === "dir" ? "d" : "-"} ${e.name}`).join("\n"),
                );
              })
              .catch(fail);
          }}
        >
          列出根目录
        </Button>
        <Button
          variant="danger"
          onClick={() => {
            onError("");
            api.fileRemove(id, path).then(onDone).catch(fail);
          }}
        >
          删除
        </Button>
      </div>
      <div className="form-grid">
        <Field label="写入内容">
          <textarea
            rows={3}
            value={content}
            onChange={(e) => {
              setContent(e.target.value);
            }}
          />
        </Field>
      </div>
      {readBack !== null && <pre className="stdout">读回：{readBack}</pre>}
      {listing !== null && <pre className="stdout">{listing}</pre>}
    </Card>
  );
}
