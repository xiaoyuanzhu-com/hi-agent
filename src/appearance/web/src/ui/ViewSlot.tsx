import { Component, useEffect, useState, type ComponentType, type ReactNode } from "react";
import { useViews } from "../core/views";
import type { Region, SizeClass, Geometry } from "../channels/out/view";
import type { Placement } from "../core/layout";

const REGIONS: readonly Region[] = [
  "center",
  "top",
  "bottom",
  "left",
  "right",
  "top_left",
  "top_right",
  "bottom_left",
  "bottom_right",
  "fill",
];
const SIZES: readonly SizeClass[] = ["compact", "auto", "wide", "fill"];

/** Read a module's self-declared `export const geometry`, keeping only known
 * enum values — a fallback placement for inline `source` views that carry no wire
 * geometry. */
function readDeclaredGeometry(mod: unknown): Geometry | undefined {
  const g = (mod as { geometry?: unknown }).geometry;
  if (!g || typeof g !== "object") return undefined;
  const region = REGIONS.find((r) => r === (g as { region?: unknown }).region);
  const size = SIZES.find((s) => s === (g as { size?: unknown }).size);
  const ownsCaptions = (g as { owns_captions?: unknown }).owns_captions;
  return {
    ...(region ? { region } : {}),
    ...(size ? { size } : {}),
    ...(typeof ownsCaptions === "boolean" ? { owns_captions: ownsCaptions } : {}),
  };
}

/**
 * Dynamically import a compiled agent view module and render its default export.
 * The module imports `react` / `@hi/core` / `motion/react` as bare specifiers,
 * resolved by the page's import map to the host's shared instances. No props: a
 * view reads the live session through `@hi/core` hooks.
 *
 * A module may also declare a fallback placement (`export const geometry`), used
 * only when the wire carried none (an inline `source` view). It's reported up on
 * every (re-)import so a `replace` under the same id can't leave a stale hint.
 */
function ViewMount({ id, moduleUrl }: { id: string; moduleUrl: string }) {
  const { reportMeta } = useViews();
  const [Comp, setComp] = useState<ComponentType | null>(null);
  const [failed, setFailed] = useState(false);

  useEffect(() => {
    let alive = true;
    setComp(null);
    setFailed(false);
    // The URL is only known at runtime; tell Vite not to try to analyze it.
    import(/* @vite-ignore */ moduleUrl)
      .then((mod) => {
        if (!alive) return;
        setComp(() => mod.default as ComponentType);
        reportMeta(id, { geometry: readDeclaredGeometry(mod) });
      })
      .catch(() => {
        if (alive) setFailed(true);
      });
    return () => {
      alive = false;
    };
  }, [id, moduleUrl, reportMeta]);

  if (failed || !Comp) return null;
  return <Comp />;
}

/** Contains a render crash in one agent view so it can't take down the host. */
class ViewErrorBoundary extends Component<{ children: ReactNode }, { crashed: boolean }> {
  constructor(props: { children: ReactNode }) {
    super(props);
    this.state = { crashed: false };
  }
  static getDerivedStateFromError() {
    return { crashed: true };
  }
  override render() {
    return this.state.crashed ? null : this.props.children;
  }
}

/**
 * The swappable region. Each active view is its own layer, keyed by view id —
 * the stable key is the animation-continuity lever (a `replace` under the same id
 * keeps the slot, so a motion-tagged element animates rather than remounting).
 * No default motion: a view appears/leaves instantly unless it opts into motion.
 *
 * Placement comes from the compositor's floor (`floorLayout`), passed in by the
 * host: a `region:"fill"` view gets the bare full-bleed layer (it owns its own
 * background and layout); any other region gets a framed, surfaced layer whose
 * `data-region`/`data-size` the CSS resolves to position + width — so a view that
 * lays out nothing of its own still lands placed and legible. A view the floor
 * didn't place falls back to the centered card.
 */
export function ViewSlot({ placements }: { placements: Map<string, Placement> }) {
  const { views } = useViews();
  if (views.length === 0) return null;
  return (
    <div style={{ position: "fixed", inset: 0, zIndex: 50 }}>
      {views.map((v) => {
        const p = placements.get(v.id);
        const mount = (
          <ViewErrorBoundary>
            <ViewMount id={v.id} moduleUrl={v.moduleUrl} />
          </ViewErrorBoundary>
        );
        if (p?.region === "fill") {
          return (
            <div key={v.id} style={{ position: "absolute", inset: 0 }}>
              {mount}
            </div>
          );
        }
        return (
          <div key={v.id} className="hi-view-frame" data-region={p?.region ?? "center"}>
            <div className="hi-view-surface" data-size={p?.size ?? "auto"}>
              {mount}
            </div>
          </div>
        );
      })}
    </div>
  );
}
