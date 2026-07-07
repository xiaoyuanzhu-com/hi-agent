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

// Visual weight of a string, budgeting caption width. CJK / full-width glyphs
// read about twice as wide as Latin, so count them double — a 30-glyph Chinese
// line and a 60-char English line take roughly the same space.
function weight(s: string): number {
  let w = 0;
  for (const ch of s) {
    w += /[\u2E80-\u9FFF\uAC00-\uD7A3\uF900-\uFAFF\uFF00-\uFFEF]/.test(ch) ? 2 : 1;
  }
  return w;
}

// Soft breakpoints *inside* a sentence — clause punctuation (kept on the clause
// they close), plus dashes and ellipses. NOT sentence terminators; those already
// split upstream in SentenceBuffer.
const CLAUSE_BREAK = /[,、，;；:：]+["'”’」』）】》)\]]*|——|……|…|—/g;

/**
 * Break one (possibly long) sentence into breath-group chunks that each fit the
 * `budget` (in weight units — ~36 CJK glyphs / ~72 Latin chars by default), so a
 * long sentence advances the caption clause-by-clause instead of parking one wall
 * of text. A sentence already within budget is returned whole (no over-chopping
 * of normal speech). A single clause that alone exceeds the budget (no soft break
 * inside it — a URL, a code token) is emitted on its own; the caption box caps its
 * height as the visual backstop.
 */
export function breakLongSentence(sentence: string, budget = 72): string[] {
  if (weight(sentence) <= budget) return [sentence];

  // Cut into clause segments at soft punctuation, keeping the punctuation on the
  // left segment (so "…修了:" stays together, not orphaned onto the next line).
  const segments: string[] = [];
  let last = 0;
  let m: RegExpExecArray | null;
  const re = new RegExp(CLAUSE_BREAK.source, "g");
  while ((m = re.exec(sentence)) !== null) {
    const end = m.index + m[0].length;
    segments.push(sentence.slice(last, end));
    last = end;
    if (re.lastIndex === m.index) re.lastIndex++; // guard against zero-width
  }
  if (last < sentence.length) segments.push(sentence.slice(last));

  // Greedily pack segments into chunks within budget.
  const chunks: string[] = [];
  let cur = "";
  for (const seg of segments) {
    if (cur && weight(cur) + weight(seg) > budget) {
      chunks.push(cur.trim());
      cur = seg;
    } else {
      cur += seg;
    }
  }
  if (cur.trim()) chunks.push(cur.trim());
  return chunks.length ? chunks : [sentence];
}
