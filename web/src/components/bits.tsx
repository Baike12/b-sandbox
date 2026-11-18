/** Small presentational primitives shared by every page. Components here do
 * no data fetching and hold no business state. */

import type { ReactNode } from "react";

export function Card({
  title,
  subtitle,
  actions,
  children,
  className,
}: {
  title?: ReactNode;
  subtitle?: ReactNode;
  actions?: ReactNode;
  children: ReactNode;
  className?: string;
}) {
  return (
    <section className={`card ${className ?? ""}`}>
      {(title !== undefined || actions !== undefined) && (
        <div className="card-head">
          <div>
            {title !== undefined && <h3>{title}</h3>}
            {subtitle !== undefined && <p className="muted small">{subtitle}</p>}
          </div>
          {actions !== undefined && <div className="card-actions">{actions}</div>}
        </div>
      )}
      {children}
    </section>
  );
}

export type Tone = "ok" | "warn" | "err" | "info" | "muted";

export function Badge({ tone, children }: { tone: Tone; children: ReactNode }) {
  return <span className={`badge badge-${tone}`}>{children}</span>;
}

export function Button({
  variant = "default",
  onClick,
  disabled,
  children,
  title,
}: {
  variant?: "default" | "primary" | "danger" | "ghost";
  onClick: () => void;
  disabled?: boolean;
  children: ReactNode;
  title?: string;
}) {
  return (
    <button
      type="button"
      className={`btn btn-${variant}`}
      onClick={onClick}
      disabled={disabled}
      title={title}
    >
      {children}
    </button>
  );
}

export function Field({
  label,
  hint,
  children,
}: {
  label: string;
  hint?: ReactNode;
  children: ReactNode;
}) {
  return (
    <label className="field">
      <span className="field-label">{label}</span>
      {children}
      {hint !== undefined && <span className="field-hint">{hint}</span>}
    </label>
  );
}

export function ErrorBanner({ message }: { message: string }) {
  return (
    <div className="error-banner" role="alert">
      <strong>请求失败</strong>
      <code>{message}</code>
    </div>
  );
}

export function Empty({ children }: { children: ReactNode }) {
  return <p className="muted empty">{children}</p>;
}

export function Spinner({ label }: { label: string }) {
  return (
    <span className="spinner-wrap">
      <span className="spinner" aria-hidden />
      {label}
    </span>
  );
}

export function ProgressRing({ done, total }: { done: number; total: number }) {
  const radius = 26;
  const circumference = 2 * Math.PI * radius;
  const offset = circumference * (1 - (total === 0 ? 0 : done / total));
  return (
    <svg
      className="progress-ring"
      width={64}
      height={64}
      viewBox="0 0 64 64"
      role="img"
      aria-label={`${done}/${total}`}
    >
      <circle className="ring-track" cx={32} cy={32} r={radius} fill="none" strokeWidth={6} />
      <circle
        className="ring-value"
        cx={32}
        cy={32}
        r={radius}
        fill="none"
        strokeWidth={6}
        strokeLinecap="round"
        strokeDasharray={circumference}
        strokeDashoffset={offset}
        transform="rotate(-90 32 32)"
      />
      <text x={32} y={37} textAnchor="middle" className="ring-text">
        {done}/{total}
      </text>
    </svg>
  );
}

export function Code({ children }: { children: ReactNode }) {
  return <code className="code">{children}</code>;
}
