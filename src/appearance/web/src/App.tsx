import { SessionProvider } from "./core";
import { ViewsProvider } from "./core/views";
import { Shell } from "./ui/Shell";

/**
 * The session providers wrap the shell so the channel loops (mic, audio, text,
 * view stream) live ABOVE the swappable view slot inside the shell — swapping a
 * view never remounts them. `ViewsProvider` sits inside `SessionProvider` because
 * it reads the scene and wake state from it.
 */
export function App() {
  return (
    <SessionProvider>
      <ViewsProvider>
        <Shell />
      </ViewsProvider>
    </SessionProvider>
  );
}
