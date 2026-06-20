# Building a view

You've been asked to build a *view* — a small React component the agent will show on
the person's screen while it talks them through it. This guide is the mechanics: what
a view is, how to author it, how to hand it back. The bar for how it should *look*
lives beside this file in `aesthetic.md` — read that too, and hold your work to it.

A view is a React component you write as JSX, the module's default export, importing
what you need as bare modules:

- `@hi/ui` — plain building blocks: `Card`, `Stack`, `Text`. Tasteful, no motion of
  their own.
- `@hi/core` — the live session as hooks: `usePresence()`, `useSpeech()`,
  `useChannels()`, `useSendText()`. Read or drive the conversation from inside a view
  with these.
- `motion/react` — Motion, when (and only when) a moment earns movement.
- `react` itself.

A minimal view:

```
import { Card, Stack, Text } from "@hi/ui";
export default function Spending() {
  return <Card><Stack><Text>Groceries crept up; everything else held steady.</Text></Stack></Card>;
}
```

Keep the view *light*. The agent shows it paced to a single spoken beat, so one view
is one idea — not a whole list crammed onto one slide. If the brief is a sequence
(a ranking, a timeline), build one view per item so the agent can walk them one at a
time; give each its own id.

**The host frames your view.** You don't lay out the whole screen. The host centers
your content in a safe-area kept clear of the live captions, the camera self-view,
and the on-screen controls, and paints a light surface behind it — so a view that
lays out nothing of its own still lands centered and readable. Return your content
directly (a `Stack`, a `Card`, your own elements); don't reach for the viewport,
full-screen backgrounds, or absolute positioning to center yourself — that fights the
frame. If your view *is* the full bleed — a photo, a map, a dark composition that owns
the whole frame — opt out:

```
export const surface = "none"; // fill the stage; you own the background and layout
```

With `"none"` the host steps back to a bare full-screen layer and the captions keep a
dark scrim so they stay legible over whatever you paint.

**Images: never hotlink.** A remote URL can fail CORS, be hotlink-blocked, or 404 —
leaving an ugly broken box. Instead **download the image into your project folder**
with your own tools (find it via web/image search, then `curl`/fetch it to a file
next to your view), and reference it by its served path: anything you save in the
views tree is served at `/views/<the same relative path>`, so a file you write to
`badminton-top10/leader.jpg` is `<img src="/views/badminton-top10/leader.jpg">`.
That path always loads and keeps your source small.

**The live words ride above your view.** While your view is on stage, the host keeps
showing the conversation's words — the person's transcribed speech and the agent's
lines — as small caption pills docked bottom-center over your view (you don't render
them). If your composition's main content lives there, move them aside by exporting a
placement from the module:

```
export const captionAside = "top"; // "top" | "bottom" | "left" | "right" | "self"
```

`"self"` means you fold the words into the composition yourself: render them with
`useSpeech()` from `@hi/core` and the host's captions stand down. Only declare it if
you actually render them — otherwise the person's speech goes invisible.

**It's theirs the moment they reach for it.** If they scroll or tap, the view should
yield — let them look, and don't fight it.

**See it before you hand it back.** You have no screen of your own, so a view you
never render is one you're shipping blind — and `aesthetic.md` holds you to *looking*
at it with the eye you'd use on someone else's work. You can: the running server (its
base URL is in `HI_AGENT_BASE_URL`) already serves, same-origin, everything a faithful
render needs — the import map injected into `GET /`, the `@hi/ui` / `@hi/core` /
`motion/react` shims it points at under `/assets/`, and any image you saved under
`/views/`. So the harness is small: compile your JSX to ESM (esbuild, bare imports
left intact, the way the host does), mount it in a headless browser — install one on
first run; it caches — against that import map, stub the live session (`@hi/core`'s
hooks: return a sample `useSpeech` line so the caption pills show), and screenshot to a
file. Then **`Read` the PNG** and fix what doesn't clear the bar before you save. Set
the harness up once in a views tool dir (say `_preview/`) and every later view
reuses it — like the browser, it resolves the first time and is ready after.

# Saving it and handing it back

When a view is ready, save it as a `.jsx` file in your views tree (your working
directory) — no special tool, just write the file. Put it in a project folder named
for the topic, with a short file name and the component as the module's default
export — e.g. `badminton-top10/leader.jsx`. Name it for what it *is*, not for today's
task, so a later you can find it by topic.

Your views tree is a workshop that accumulates across tasks — everything saved here
stays. Before authoring from scratch, glance at it (`ls`): partly so you don't
collide with an existing project, but mostly because the quickest, most consistent
build is often one you already have. If you — or an earlier you — made something
close, the same kind of card, last month's version of this very deck, start from it
and adapt rather than redrawing it cold. That reuse is how the workshop earns its
keep: the stock you build up is yours to draw on, and it keeps the house style
consistent for free.

The view's *ref* is that path without the `.jsx` — `badminton-top10/leader`. Report
every ref you saved back to the agent in your summary — that's the only way the agent
can put your view on screen (it calls `show_view` with the ref). If you built several
views for one presentation, save each as its own file under the project folder and
list all the refs in order, so the agent can walk them as a sequence.
