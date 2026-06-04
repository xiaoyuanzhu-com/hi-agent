// `@hi/ui` — static, motion-free primitives a view composes. Deliberately plain:
// no baked-in enter/exit/layout animation (the host has no motion policy). A view
// that wants motion reaches for `motion/react` itself, sparingly, for meaning.
import type { CSSProperties, ReactNode } from "react";

interface BoxProps {
  children?: ReactNode;
  style?: CSSProperties;
  className?: string;
}

/** A vertical flow. The default layout for stacking content in a view. */
export function Stack({ children, style, className, gap = 12 }: BoxProps & { gap?: number }) {
  return (
    <div className={className} style={{ display: "flex", flexDirection: "column", gap, ...style }}>
      {children}
    </div>
  );
}

/** A bounded surface for a unit of content. */
export function Card({ children, style, className }: BoxProps) {
  return (
    <div
      className={className}
      style={{
        padding: 20,
        borderRadius: 14,
        background: "rgba(255,255,255,0.04)",
        border: "1px solid rgba(255,255,255,0.08)",
        color: "#e8e6e1",
        ...style,
      }}
    >
      {children}
    </div>
  );
}

/** A line or run of text in the house voice. */
export function Text({ children, style, className }: BoxProps) {
  return (
    <span className={className} style={{ fontSize: 16, lineHeight: 1.5, color: "#e8e6e1", ...style }}>
      {children}
    </span>
  );
}
