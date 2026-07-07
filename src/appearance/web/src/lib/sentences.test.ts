import { describe, it, expect } from "vitest";
import { SentenceBuffer, breakLongSentence } from "./sentences";

describe("SentenceBuffer", () => {
  it("emits a sentence once a terminator + space arrives, across chunks", () => {
    const b = new SentenceBuffer();
    expect(b.push("Hello world")).toEqual([]);
    expect(b.push(". How are")).toEqual(["Hello world."]);
    expect(b.push(" you? Good")).toEqual(["How are you?"]);
    expect(b.flush()).toEqual(["Good"]);
  });

  it("does not split decimals or trailing-period without whitespace", () => {
    const b = new SentenceBuffer();
    expect(b.push("pi is 3.14 today")).toEqual([]); // 3.14 not split
  });

  it("splits CJK terminators immediately (no spaces needed)", () => {
    const b = new SentenceBuffer();
    expect(b.push("你好。最近怎么样？")).toEqual(["你好。", "最近怎么样？"]);
  });

  it("treats newlines as boundaries", () => {
    const b = new SentenceBuffer();
    expect(b.push("one\n\ntwo three")).toEqual(["one"]);
    expect(b.flush()).toEqual(["two three"]);
  });

  it("emits multiple complete sentences in one push", () => {
    const b = new SentenceBuffer();
    expect(b.push("A cat sat. A dog ran! Then ")).toEqual([
      "A cat sat.",
      "A dog ran!",
    ]);
  });

  it("reset() drops the buffered partial", () => {
    const b = new SentenceBuffer();
    b.push("half a thought");
    b.reset();
    expect(b.flush()).toEqual([]);
  });
});

describe("breakLongSentence", () => {
  it("returns a within-budget sentence whole (no over-chopping)", () => {
    expect(breakLongSentence("你好,最近怎么样")).toEqual(["你好,最近怎么样"]);
    expect(breakLongSentence("A short reply, nothing to split.")).toEqual([
      "A short reply, nothing to split.",
    ]);
  });

  it("splits a long CJK sentence at clause boundaries, losslessly", () => {
    const long =
      "查的过程中抓到一个真会出事的点,已经顺手修了:node-exporter 默认要占主机的 9100 端口,可 bj-01 上 traefik 已经占着 9100 了,原样部署它会起不来。";
    const chunks = breakLongSentence(long);
    expect(chunks.length).toBeGreaterThan(1); // no wall of text
    expect(chunks.join("")).toBe(long); // every character preserved (CJK: no spaces)
    // Clause punctuation stays on the clause it closes — chunks break after it,
    // so a non-final chunk ends on a soft terminator, never mid-word.
    for (const c of chunks.slice(0, -1)) {
      expect(/[,、，;；:：]$/.test(c)).toBe(true);
    }
  });

  it("splits a long Latin sentence at its commas", () => {
    const long =
      "first clause here, second clause here, third clause here, fourth clause here";
    const chunks = breakLongSentence(long, 24);
    expect(chunks.length).toBeGreaterThan(1);
    expect(chunks.every((c) => c === c.trim() && c.length > 0)).toBe(true);
  });

  it("emits an unbreakable over-budget clause on its own (backstop is the CSS cap)", () => {
    const token = "verylongunbrokenslugwithnosoftbreakwhatsoeverkeepsgoingandgoing";
    expect(breakLongSentence(token, 20)).toEqual([token]);
  });
});
