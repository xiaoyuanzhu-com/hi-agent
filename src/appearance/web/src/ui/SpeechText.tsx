export interface SpeechItem {
  id: number;
  text: string;
  /** Who said it — the agent's reply (default) or the user's transcribed speech. */
  speaker?: "agent" | "user";
  /** True while a user line is still a rolling preliminary (not yet polished). */
  pending?: boolean;
}

interface SpeechTextProps {
  /** The visible window of recent lines; newest last. */
  items: SpeechItem[];
}

/**
 * The conversation's words as calm, whole-sentence fades. Each line fades +
 * rises in as a settled whole (never letter-by-letter); the previous one dims
 * behind it. The user's own transcribed speech shares this area, marked apart
 * from the agent's reply and shown live (rolling) until it's polished.
 */
export function SpeechText({ items }: SpeechTextProps) {
  return (
    <div className="hi-speech" aria-live="polite">
      {items.map((it, i) => {
        const isCurrent = i === items.length - 1;
        const cls = [
          "hi-sentence",
          isCurrent ? "" : "hi-sentence--prev",
          it.speaker === "user" ? "hi-sentence--user" : "",
          it.pending ? "hi-sentence--pending" : "",
        ]
          .filter(Boolean)
          .join(" ");
        return (
          <p key={it.id} className={cls}>
            {it.text}
          </p>
        );
      })}
    </div>
  );
}
