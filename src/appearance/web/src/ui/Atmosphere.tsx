/**
 * A faint film of grain laid over the presence field (see Presence.tsx, which
 * renders the breathing background on a canvas beneath this layer). Grain alone
 * keeps the near-black field from banding; it never competes with the words or
 * content. Pure CSS — see `.hi-atmo-grain` in global.css.
 */
export function Atmosphere() {
  return (
    <div aria-hidden className="hi-atmosphere">
      <div className="hi-atmo-grain" />
    </div>
  );
}
