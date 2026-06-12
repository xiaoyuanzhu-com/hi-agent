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

**Images: never hotlink.** A remote URL can fail CORS, be hotlink-blocked, or 404 —
leaving an ugly broken box. Instead **download the image into your project folder**
with your own tools (find it via web/image search, then `curl`/fetch it to a file
next to your view), and reference it by its served path: anything you save in the
workspace is served at `/workspace/<the same relative path>`, so a file you write to
`badminton-top10/leader.jpg` is `<img src="/workspace/badminton-top10/leader.jpg">`.
That path always loads and keeps your source small.

**It's theirs the moment they reach for it.** If they scroll or tap, the view should
yield — let them look, and don't fight it.

# Saving it and handing it back

When a view is ready, save it as a `.jsx` file in your workspace (your working
directory) — no special tool, just write the file. Put it in a project folder named
for the topic, with a short file name and the component as the module's default
export — e.g. `badminton-top10/leader.jsx`. Glance at the workspace (`ls`) first so
you don't collide with an existing project.

The view's *ref* is that path without the `.jsx` — `badminton-top10/leader`. Report
every ref you saved back to the agent in your summary — that's the only way the agent
can put your view on screen (it calls `show_view` with the ref). If you built several
views for one presentation, save each as its own file under the project folder and
list all the refs in order, so the agent can walk them as a sequence.
