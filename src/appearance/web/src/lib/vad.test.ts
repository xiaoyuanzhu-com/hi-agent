import { describe, it, expect } from "vitest";
import { Vad } from "./vad";

const opts = { startThreshold: 0.04, endThreshold: 0.02, endSilenceMs: 700, minVoicedMs: 300 };

describe("Vad", () => {
  it("fires start when amplitude crosses the start threshold", () => {
    const v = new Vad(opts);
    expect(v.push(0.01, 100)).toBeNull();
    expect(v.push(0.05, 100)).toEqual({ type: "start" });
    expect(v.active).toBe(true);
  });

  it("ends after sustained silence and reports voiced duration", () => {
    const v = new Vad(opts);
    v.push(0.05, 100); // start, voiced=100
    v.push(0.06, 100);
    v.push(0.06, 100);
    v.push(0.06, 100); // voiced=400
    expect(v.push(0.0, 300)).toBeNull(); // silence 300
    expect(v.push(0.0, 300)).toBeNull(); // silence 600
    expect(v.push(0.0, 200)).toEqual({ type: "end", voicedMs: 400, droppedTooShort: false }); // 800 >= 700
    expect(v.active).toBe(false);
  });

  it("flags too-short utterances", () => {
    const v = new Vad(opts);
    v.push(0.05, 100); // start, voiced=100
    const e = v.push(0.0, 700); // 700 silence -> end, voiced=100 < 300
    expect(e).toEqual({ type: "end", voicedMs: 100, droppedTooShort: true });
  });

  it("resets the silence timer on re-voicing", () => {
    const v = new Vad(opts);
    v.push(0.05, 100);
    v.push(0.0, 400); // silence 400
    v.push(0.05, 100); // re-voiced -> silence resets
    expect(v.push(0.0, 400)).toBeNull(); // only 400 again, no end
    expect(v.active).toBe(true);
  });
});
