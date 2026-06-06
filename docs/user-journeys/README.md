# User Journeys

A living catalog of concrete cases and their **expected UX**. Each file documents
one journey: what the user is trying to do, what they see and do step by step, and
what the agent/system is expected to do in response.

These are the source of truth for *intended* behavior — write the expected UX here
first, then build/verify against it. When behavior and a journey disagree, that's a
bug in one or the other; resolve it explicitly rather than silently.

## How to add a journey

1. Copy the structure below into a new file: `NN-short-slug.md` (e.g. `01-first-launch.md`).
2. Keep it concrete — real clicks, real screens, real messages, not abstractions.
3. Describe expected UX, not implementation. Link to architecture/code only when it clarifies.

## Template

```markdown
# <Journey title>

**Persona:** who is doing this (and what they already know)
**Goal:** what they want to accomplish
**Preconditions:** what must be true before this starts

## Steps & expected UX

1. **User does X** → system/agent responds with Y (what they see, hear, feel).
2. ...

## Expected outcome

What "done" looks like, and how the user knows it worked.

## Edge cases & failure modes

- What happens when <thing goes wrong> → expected handling.

## Open questions

- Anything undecided about the intended UX.
```

## Index

- [01 · 羽毛球男单世界前十](01-badminton-top10.md) — 打招呼 → 异步检索演示前十 → 钻取单个球员 → 切换到天气;确立对话简短、音画结合演示、窗口式轮播、柔和转场等通用原则。
