/**
 * A faint film of grain over the themed paper (`.hi-atmo-grain` in global.css),
 * to keep large flat gradient areas from banding. It never competes with the
 * words or content.
 *
 * The old frosted-white glass sheet lived here too — it blurred the watercolour
 * canvas behind it so the pools bloomed *through* glass. With the flat themed
 * paper (Presence.tsx) there is nothing to frost, so the glass is gone; only the
 * grain remains.
 */
export function Atmosphere() {
  return (
    <div aria-hidden className="hi-atmosphere">
      <div className="hi-atmo-grain" />
    </div>
  );
}
