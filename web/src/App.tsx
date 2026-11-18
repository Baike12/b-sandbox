/** Root composition: hash routing, health polling, onboarding state with
 * persistence, welcome dialog, and the first-run tour. */

import { useCallback, useEffect, useMemo, useState } from "react";
import { api, ApiError, hasToken, setToken } from "./api/client";
import { Shell } from "./components/Shell";
import { Tour } from "./components/Tour";
import type { TourStep } from "./components/Tour";
import { Button, Card } from "./components/bits";
import { loadOnboarding, reduce, saveOnboarding } from "./logic/onboarding";
import type { OnboardingEvent, OnboardingState } from "./logic/onboarding";
import { parseRoute, routeToHash } from "./logic/router";
import type { PageName, Route } from "./logic/router";
import { Create } from "./pages/Create";
import { Guide } from "./pages/Guide";
import { Metrics } from "./pages/Metrics";
import { Sandboxes } from "./pages/Sandboxes";
import type { Health } from "./api/types";

const TOUR_STEPS: readonly TourStep[] = [
  {
    selector: "[data-tour='tour-health']",
    title: "服务健康徽章",
    body: "实时显示 /health 的结果：运行中（后端可用）、降级（后端缺失，服务拒绝降级执行）或无法连接。旁边是令牌状态。",
  },
  {
    selector: "[data-tour='tour-token']",
    title: "访问令牌",
    body: "服务以 --auth-token 启动时，点「设置」粘贴同一个令牌。令牌只存本浏览器，只发给本服务。",
  },
  {
    selector: "[data-tour='tour-nav']",
    title: "四个页面",
    body: "上手指引（本清单）、沙箱管理（执行/文件/事件）、创建沙箱（策略配置）、指标与评测（评测标准 + 自测）。左下角有文档与源码链接。",
  },
  {
    selector: "[data-tour='tour-replay']",
    title: "随时回来看指引",
    body: "顶栏「指引」可重新播放本导览；首页的任务卡清单会跟踪你的上手进度。",
  },
];

export function App() {
  const [route, setRoute] = useState<Route>(() => parseRoute(window.location.hash));
  const [onboarding, setOnboarding] = useState<OnboardingState>(() => loadOnboarding());
  const [health, setHealth] = useState<Health | null>(null);
  const [healthError, setHealthError] = useState<string | null>(null);
  const [tokenSet, setTokenSet] = useState<boolean>(hasToken());
  const [tourActive, setTourActive] = useState(false);

  // Hash → route. The listener owns route; nothing else writes it.
  useEffect(() => {
    const onHash = () => {
      setRoute(parseRoute(window.location.hash));
    };
    window.addEventListener("hashchange", onHash);
    return () => {
      window.removeEventListener("hashchange", onHash);
    };
  }, []);

  const dispatch = useCallback((event: OnboardingEvent) => {
    setOnboarding((prev) => {
      const next = reduce(prev, event);
      saveOnboarding(next);
      return next;
    });
  }, []);

  // Health poll: every 5 s, forever. A successful scrape completes the
  // first onboarding step.
  useEffect(() => {
    let alive = true;
    const poll = () => {
      api
        .health()
        .then((h) => {
          if (!alive) {
            return;
          }
          setHealth(h);
          setHealthError(null);
          dispatch({ type: "service_reachable" });
        })
        .catch((e: unknown) => {
          if (!alive) {
            return;
          }
          setHealth(null);
          setHealthError(e instanceof ApiError ? e.message : String(e));
        });
    };
    poll();
    const timer = window.setInterval(poll, 5000);
    return () => {
      alive = false;
      window.clearInterval(timer);
    };
  }, [dispatch]);

  // First visit: welcome dialog once.
  useEffect(() => {
    if (!onboarding.welcomed && health !== null) {
      dispatch({ type: "welcome_shown" });
    }
  }, [onboarding.welcomed, health, dispatch]);

  const navigate = useCallback((page: PageName) => {
    window.location.hash = routeToHash(
      page === "sandboxes" ? { page: "sandboxes", selected: null } : { page },
    );
  }, []);

  const content = useMemo(() => {
    switch (route.page) {
      case "guide":
        return (
          <Guide
            state={onboarding}
            dispatch={dispatch}
            health={health}
            onNavigate={navigate}
            onReplayTour={() => {
              setTourActive(true);
            }}
          />
        );
      case "sandboxes":
        return (
          <Sandboxes
            selected={route.selected}
            onExecRan={() => {
              dispatch({ type: "exec_ran" });
            }}
          />
        );
      case "create":
        return (
          <Create
            onCreated={() => {
              dispatch({ type: "sandbox_created" });
            }}
          />
        );
      case "metrics":
        return (
          <Metrics
            onSelfTestRan={() => {
              dispatch({ type: "self_test_ran" });
            }}
          />
        );
    }
  }, [route, onboarding, dispatch, health, navigate]);

  // The tour runs on demand (header button / first-visit offer).
  return (
    <Shell
      health={health}
      healthError={healthError}
      page={route.page}
      onNavigate={navigate}
      tokenSet={tokenSet}
      onSaveToken={(token) => {
        setToken(token);
        setTokenSet(token !== "");
      }}
      onReplayTour={() => {
        setTourActive(true);
      }}
    >
      {content}
      {tourActive && (
        <Tour
          steps={TOUR_STEPS}
          onDone={() => {
            setTourActive(false);
            dispatch({ type: "tour_finished" });
          }}
        />
      )}
      {!tourActive && onboarding.welcomed && !onboarding.tourDone && (
        <div className="tour-offer">
          <Card title="第一次来？">
            <p className="muted">用 30 秒看一遍页面导览，或直接从上面的四步清单开始。</p>
            <div className="tour-actions">
              <Button
                variant="primary"
                onClick={() => {
                  setTourActive(true);
                }}
              >
                开始导览
              </Button>
              <Button
                variant="ghost"
                onClick={() => {
                  dispatch({ type: "tour_finished" });
                }}
              >
                跳过
              </Button>
            </div>
          </Card>
        </div>
      )}
    </Shell>
  );
}
