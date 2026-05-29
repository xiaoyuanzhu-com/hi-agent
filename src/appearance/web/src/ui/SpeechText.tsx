export interface SpeechItem {
  id: number;
  text: string;
}

interface SpeechTextProps {
  /** The visible window of recent sentences; newest last. */
  items: SpeechItem[];
}

/**
 * The agent's words as calm, whole-sentence fades. Each sentence fades + rises
 * in as a settled whole (never letter-by-letter); the previous one dims behind
 * it. The session feeds this a short rolling window (1–2 sentences).
 */
export function SpeechText({ items }: SpeechTextProps) {
  return (
    <div className="hi-speech" aria-live="polite">
      {items.map((it, i) => {
        const isCurrent = i === items.length - 1;
        return (
          <p
            key={it.id}
            className={isCurrent ? "hi-sentence" : "hi-sentence hi-sentence--prev"}
          >
            {it.text}
          </p>
        );
      })}
    </div>
  );
}
