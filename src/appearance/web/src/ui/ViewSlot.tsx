import { Component, useEffect, useState, type ComponentType, type ReactNode } from "react";
import { useViews, type CaptionAside, type ViewSurface } from "../core/views";

const CAPTION_ASIDES: readonly CaptionAside[] = ["top", "bottom", "left", "right", "self"];
const SURFACES: readonly ViewSurface[] = ["card", "none"];

/**
 * Dynamically import a compiled agent view module and render its default export.
 * The module imports `react` / `@hi/core` / `motion/react` as bare specifiers,
 * resolved by the page's import map to the host's shared instances. No props: a
 * view reads the live session through `@hi/core` hooks.
 *
 * A module may also declare host-framing hints (`export const captionAside`, where
 * the live captions dock; `export const surface`, whether the host frames + surfaces
 * its content). Both are reported up on every (re-)import so a `replace` under the
 * same id can't leave a stale hint behind.
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
        const declaredAside = (mod as { captionAside?: unknown }).captionAside;
        const aside = CAPTION_ASIDES.find((a) => a === declaredAside);
        const declaredSurface = (mod as { surface?: unknown }).surface;
        const surface = SURFACES.find((s) => s === declaredSurface);
        reportMeta(id, { ...(aside ? { captionAside: aside } : {}), ...(surface ? { surface } : {}) });
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
 * The host frames each view unless it opts out (`surface: "none"`): a centered
 * safe-area (`.hi-view-frame`, kept clear of the captions / camera pip / controls)
 * and a legible surface (`.hi-view-surface`) — so a view that lays out nothing of
 * its own still lands centered and readable rather than flush at an edge. The
 * caption side it itself declared drives the reserved strip, so its content can't
 * collide with its own captions. `surface: "none"` returns the bare full-bleed
 * layer for views that own their whole frame.
 */
export function ViewSlot() {
  const { views, meta } = useViews();
  if (views.length === 0) return null;
  return (
    <div style={{ position: "fixed", inset: 0, zIndex: 50 }}>
      {views.map((v) => {
        const m = meta.get(v.id);
        const mount = (
          <ViewErrorBoundary>
            <ViewMount id={v.id} moduleUrl={v.moduleUrl} />
          </ViewErrorBoundary>
        );
        if (m?.surface === "none") {
          return (
            <div key={v.id} style={{ position: "absolute", inset: 0 }}>
              {mount}
            </div>
          );
        }
        return (
          <div key={v.id} className="hi-view-frame" data-aside={m?.captionAside ?? "bottom"}>
            <div className="hi-view-surface">{mount}</div>
          </div>
        );
      })}
    </div>
  );
}
