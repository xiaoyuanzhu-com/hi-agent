import { describe, it, expect } from "vitest";
import { SentenceBuffer } from "./sentences";

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
