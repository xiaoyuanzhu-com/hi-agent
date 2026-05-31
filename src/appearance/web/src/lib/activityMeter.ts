// ActivityMeter — a decaying 0..1 intensity the Presence can read each frame.
//
// Cognition is a black box until tokens land, so the one *live* signal we have
// for thinking is the cadence of streamed thought chunks. Each chunk `bump()`s
// the meter; between chunks it decays on a fixed half-life. A steady stream of
// tokens holds the meter high, a pause lets it fade — so the field pulses with
// the agent's real output rate the way it already rides the live voice.

export class ActivityMeter {
  private value = 0;
  private last = performance.now();
  private readonly halfLifeMs: number;

  constructor(halfLifeMs = 700) {
    this.halfLifeMs = halfLifeMs;
  }

  /** Register activity (e.g. a streamed chunk). `amount` adds to the meter. */
  bump(amount = 1): void {
    this.decay();
    this.value = Math.min(1, this.value + amount);
  }

  /** Current 0..1 intensity, decayed to now. */
  read(): number {
    this.decay();
    return this.value;
  }

  private decay(): void {
    const now = performance.now();
    const dt = now - this.last;
    this.last = now;
    if (dt > 0) this.value *= Math.pow(0.5, dt / this.halfLifeMs);
  }
}
