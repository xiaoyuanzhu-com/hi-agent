import { describe, expect, it } from "vitest";

import { type RawHistoryMsg, historyToMessages, mediaKindFromMime } from "./model";

describe("mediaKindFromMime", () => {
  it("classifies by mime prefix", () => {
    expect(mediaKindFromMime("image/png")).toBe("image");
    expect(mediaKindFromMime("audio/mpeg")).toBe("audio");
    expect(mediaKindFromMime("video/mp4")).toBe("video");
    expect(mediaKindFromMime("application/pdf")).toBe("file");
    expect(mediaKindFromMime("")).toBe("file");
  });
});

describe("historyToMessages", () => {
  it("maps dir, body, and ts", () => {
    const raw: RawHistoryMsg[] = [
      { id: "a", ts: "2026-06-25T09:00:00Z", dir: "in", channel: "text", body: "hi" },
      { id: "b", ts: "2026-06-25T09:00:01Z", dir: "out", channel: "text", body: "hello" },
    ];
    const got = historyToMessages(raw);
    expect(got.map((m) => [m.id, m.dir, m.text])).toEqual([
      ["a", "in", "hi"],
      ["b", "out", "hello"],
    ]);
    expect(got[0]!.ts).toBe(Date.parse("2026-06-25T09:00:00Z"));
    expect(got[0]!.media).toBeUndefined();
  });

  it("carries media and renames duration_ms → durationMs", () => {
    const raw: RawHistoryMsg[] = [
      {
        id: "c",
        ts: "2026-06-25T09:00:02Z",
        dir: "in",
        channel: "audio",
        body: "spoken words",
        media: { url: "/api/media?x=1", mime: "audio/mpeg", kind: "audio", duration_ms: 4200 },
      },
    ];
    const [m] = historyToMessages(raw);
    expect(m!.media).toEqual({
      url: "/api/media?x=1",
      mime: "audio/mpeg",
      kind: "audio",
      name: undefined,
      width: undefined,
      height: undefined,
      durationMs: 4200,
    });
  });
});
