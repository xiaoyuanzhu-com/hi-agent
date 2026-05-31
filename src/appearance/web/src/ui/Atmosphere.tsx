/**
 * The layers laid over the presence field (see Presence.tsx, which renders the
 * drifting colour-pools on a canvas beneath this).
 *
 *  - `.hi-glass` — a frosted white sheet that `backdrop-filter: blur()`s the
 *    canvas behind it, so the pools read as soft light blooming *through* glass
 *    rather than as flat shapes. This is what gives the lightweight, airy feel;
 *    the words (one layer up) sit cleanly on the bright frosted surface.
 *  - `.hi-atmo-grain` — a faint film of grain over the glass to keep large flat
 *    areas from banding. It never competes with the words or content.
 *
 * Pure CSS — see `.hi-glass` / `.hi-atmo-grain` in global.css.
 */
export function Atmosphere() {
  return (
    <div aria-hidden className="hi-atmosphere">
      <div className="hi-glass" />
      <div className="hi-atmo-grain" />
    </div>
  );
}
