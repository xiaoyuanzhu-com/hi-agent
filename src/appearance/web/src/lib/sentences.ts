// Incremental sentence splitter for the calm whole-sentence text fade.
//
// `/thought` arrives as arbitrary chunks. We buffer and emit only *complete*
// sentences, so SpeechText can fade each one in as a settled whole rather than
// letter-by-letter.
//
// Two boundary rules, because CJK and Latin punctuate differently:
//   * CJK terminators (。！？) split immediately — CJK text isn't space-separated,
//     so waiting for whitespace would never fire.
//   * Latin terminators (.!?…) split only when followed by whitespace, so we
//     don't break "3.14" or "U.S." mid-token. The trailing partial stays
//     buffered until the next chunk brings the space, or until flush().

function boundaryRe(): RegExp {
  // 1) CJK terminator (+ optional CJK closers)
  // 2) Latin terminator (+ optional closers) followed by whitespace (lookahead)
  // 3) a run of newlines
  return /[。！？]+["'”’」』）】》]*|[.!?…]+["'”’)\]]*(?=\s)|\n+/g;
}

export class SentenceBuffer {
  private buf = "";

  /** Feed a raw chunk; returns any sentences that just completed. */
  push(chunk: string): string[] {
    this.buf += chunk;
    const re = boundaryRe();
    const out: string[] = [];
    let last = 0;
    let m: RegExpExecArray | null;
    while ((m = re.exec(this.buf)) !== null) {
      const end = m.index + m[0].length;
      const sentence = this.buf.slice(last, end).trim();
      if (sentence) out.push(sentence);
      last = end;
      if (re.lastIndex === m.index) re.lastIndex++; // guard against zero-width
    }
    this.buf = this.buf.slice(last);
    return out;
  }

  /** Emit whatever remains — the final, unterminated sentence. */
  flush(): string[] {
    const tail = this.buf.trim();
    this.buf = "";
    return tail ? [tail] : [];
  }

  /** Drop buffered state (e.g. on interruption). */
  reset(): void {
    this.buf = "";
  }
}
