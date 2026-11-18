/** Spotlight tour (first-run coach marks). Anchored to elements by a CSS
 * selector; each step dims everything except the target and places a card
 * next to it. Steps whose selector cannot be found are skipped, so layout
 * changes degrade to a shorter tour instead of a broken one. */

import { useEffect, useState } from "react";
import { Button } from "./bits";

export interface TourStep {
  /** CSS selector of the element to spotlight. */
  selector: string;
  title: string;
  body: string;
}

interface Rect {
  top: number;
  left: number;
  width: number;
  height: number;
}

function rectOf(selector: string): Rect | null {
  const el = document.querySelector(selector);
  if (el === null) {
    return null;
  }
  const r = el.getBoundingClientRect();
  if (r.width === 0 && r.height === 0) {
    return null;
  }
  return { top: r.top, left: r.left, width: r.width, height: r.height };
}

const PAD = 6;

export function Tour({ steps, onDone }: { steps: readonly TourStep[]; onDone: () => void }) {
  const [index, setIndex] = useState(0);
  const [rect, setRect] = useState<Rect | null>(null);

  useEffect(() => {
    const apply = () => {
      setRect(index < steps.length ? rectOf(steps[index]?.selector ?? "") : null);
    };
    apply();
    window.addEventListener("resize", apply);
    return () => {
      window.removeEventListener("resize", apply);
    };
  }, [index, steps]);

  // A missing target means the step cannot be shown: skip it immediately.
  useEffect(() => {
    if (rect === null && index < steps.length) {
      setIndex((i) => Math.min(i + 1, steps.length));
    }
  }, [rect, index, steps.length]);

  if (index >= steps.length) {
    return null;
  }
  const step = steps[index];
  if (rect === null) {
    return null;
  }

  // Tooltip below the target, above when there is no room below.
  const tooltipWidth = 320;
  const left = Math.min(Math.max(12, rect.left), window.innerWidth - tooltipWidth - 12);
  const below = rect.top + rect.height + 14;
  const tooltipTop = below + 190 < window.innerHeight ? below : Math.max(12, rect.top - 200);

  return (
    <div className="tour-layer">
      <div
        className="tour-spot"
        style={{
          top: rect.top - PAD,
          left: rect.left - PAD,
          width: rect.width + PAD * 2,
          height: rect.height + PAD * 2,
        }}
      />
      <div
        className="tour-card"
        style={{ top: tooltipTop, left }}
        role="dialog"
        aria-label={step.title}
      >
        <h4>
          {step.title}
          <span className="muted small">
            {" "}
            · {index + 1}/{steps.length}
          </span>
        </h4>
        <p>{step.body}</p>
        <div className="tour-actions">
          <Button variant="ghost" onClick={onDone}>
            跳过导览
          </Button>
          <span className="spacer" />
          {index > 0 && (
            <Button
              onClick={() => {
                setIndex(index - 1);
              }}
            >
              上一步
            </Button>
          )}
          <Button
            variant="primary"
            onClick={() => {
              if (index + 1 >= steps.length) {
                onDone();
              } else {
                setIndex(index + 1);
              }
            }}
          >
            {index + 1 >= steps.length ? "完成" : "下一步"}
          </Button>
        </div>
      </div>
    </div>
  );
}
