import { describe, it, expect } from "vitest";
import { floorLayout, CAPTIONS_ID, CAMERA_ID, type Participant } from "./layout";
import type { Geometry } from "../channels/out/view";

// The floor is the no-regression contract: with no view declaring geometry, it
// must reproduce the host's prior hand-written placement (Shell's old
// overlaid/docked/aside/pip/demote logic) exactly. These five cases lock that.

const captions = (): Participant => ({ id: CAPTIONS_ID, kind: "captions" });
const camera = (): Participant => ({ id: CAMERA_ID, kind: "camera" });
const view = (id: string, geometry?: Geometry): Participant => ({ id, kind: "view", geometry });

describe("floorLayout", () => {
  it("(a) no views, camera off → captions centered, full list, presence undimmed", () => {
    const { demote, placements } = floorLayout([captions()]);
    expect(demote).toBe(0);
    const cap = placements.get(CAPTIONS_ID)!;
    expect(cap.region).toBe("center");
    expect(cap.docked).toBe(false);
    expect(cap.hidden).toBe(false);
  });

  it("(b) no views, camera on → camera fills, captions dock bottom-center", () => {
    const { demote, placements } = floorLayout([captions(), camera()]);
    expect(demote).toBe(0);
    const cam = placements.get(CAMERA_ID)!;
    expect(cam.region).toBe("fill");
    expect(cam.pip).toBe(false);
    const cap = placements.get(CAPTIONS_ID)!;
    expect(cap.region).toBe("bottom");
    expect(cap.docked).toBe(true);
  });

  it("(c) one undeclared view → centered card, captions dock bottom, demote 0.72, camera→pip", () => {
    const { demote, placements } = floorLayout([view("v1"), captions(), camera()]);
    expect(demote).toBe(0.72);
    const v = placements.get("v1")!;
    expect(v.region).toBe("center");
    expect(v.size).toBe("auto");
    const cap = placements.get(CAPTIONS_ID)!;
    expect(cap.region).toBe("bottom");
    expect(cap.docked).toBe(true);
    expect(cap.hidden).toBe(false);
    const cam = placements.get(CAMERA_ID)!;
    expect(cam.pip).toBe(true);
    expect(cam.region).toBe("bottom_left");
  });

  it("(d) view region:fill → bare full-bleed layer (old surface:none)", () => {
    const { placements } = floorLayout([view("v1", { region: "fill", size: "fill" }), captions()]);
    const v = placements.get("v1")!;
    expect(v.region).toBe("fill");
    expect(v.size).toBe("fill");
    // Captions still land clear of a full-bleed view, at the bottom.
    expect(placements.get(CAPTIONS_ID)!.region).toBe("bottom");
  });

  it("(e) view owns_captions → host captions stand down (old selfHosted)", () => {
    const { placements } = floorLayout([view("v1", { owns_captions: true }), captions()]);
    expect(placements.get(CAPTIONS_ID)!.hidden).toBe(true);
  });

  it("places a declared view at its region/size; captions still dock bottom-center", () => {
    const { placements } = floorLayout([view("v1", { region: "left", size: "wide" }), captions()]);
    const v = placements.get("v1")!;
    expect(v.region).toBe("left");
    expect(v.size).toBe("wide");
    // The words no longer follow the view to a free edge — one fixed bottom dock.
    expect(placements.get(CAPTIONS_ID)!.region).toBe("bottom");
  });
});
