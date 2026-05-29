/**
 * The background "room" — a near-black field with a slow-drifting radial glow
 * and faint grain that breathes on a ~16s cycle. Pure CSS (see global.css);
 * never competes with the presence or content.
 */
export function Atmosphere() {
  return (
    <div aria-hidden className="hi-atmosphere">
      <div className="hi-atmo-glow" />
      <div className="hi-atmo-grain" />
    </div>
  );
}
