# 干到一半被打断,重启后自己接着干完

**Persona:** 老板(用户),正用着某个应用(在 MyLifeDB 对话里、或在看文章)随手选中了几个生词,让 agent 收进生词本、做成记忆卡片摆上屏幕。和 agent 是老板-员工关系:老板出任务,员工干全部实际的活。
**Goal:** 一笔**一次性交付**(不是常驻盯守):把选中的生词做成卡片摆上屏幕。但交付还没完成,host 进程就重启了(`make dev` Ctrl-C 再起 / 崩溃 / 主机重启)——agent 应当在重启后**自己想起这件没干完的事**,判断还需不需要,然后接着干完(或妥帖地重新浮现),而不是把它悄悄丢了。
**Preconditions:** hi-agent 在跑;agent 已经接了一个有交付物的活,并派了 worker 去做(worker 正在渲染卡片);此刻进程被打断。

> 本 journey 提炼自 2026-06-24 的真实片段(scene `z2tysdcx`:老板让收四个生词 amnesia/transient/substrate/stratum 做成卡片,worker 正在做,老板 `make dev` Ctrl-C 重启)。它与 [02](02-feishu-sprint-backlog.md)/[03](03-feishu-flash-cards.md)/[05](05-news-and-watch.md) 的"重启自恢复"是**孪生但不同的一类**:那三个恢复的是**常驻职责**(让监听/盯守活着);这里恢复的是**做到一半的一次性交付**(欠老板的那几张卡)。机制同源(commitments + 读记忆-醒来-注意),但 UX 场景不同——这里没有"永远盯着"的承诺,只有"这件事我答应了还没做完"。

---

## 完整经过(从用户视角)

> 现状列:✅ 最新实测达标 · 🟡 引导已发、未复测(soft guidance 只改概率,等下次 journey 实测打分) · ⚠️ 已知缺口。

| 幕 | 发生什么 | agent 的预期行为 | 原则 | 现状 2026-06-25 |
|---|---|---|---|---|
| **1 · 接活,记下欠的** | 老板:"把这几个生词做成卡片摆上来" | 接住;先把词存进生词本(**数据当场落盘**),派 worker 去渲染卡片,口头确认"在做了,马上摆上来"。**接活的当下**把这笔欠的交付记进 commitments.md 作为一条 open loop:欠老板四张生词卡片 | 半成品交付 = 一条**临时承诺**;和常驻职责同一个文件、同一套机制,落即记 | 🟡 引导已发(core.md "Your own operation"),未实测 |
| **2 · 被打断** | 进程重启(Ctrl-C 再起 / 崩溃 / 主机重启)。worker 连同它的内存状态一起没了,卡片没摆出来 | (无)——worker 只活在内存(`tokio::spawn` + HashMap),进程一死就没了;能留下的只有落盘的:生词本里的词、journal 里的对话、views/ 里建了一半的产物 | 只活在内存里的恢复不了;能恢复的前提是状态可从持久 journal + 落盘产物重建 | — |
| **3 · 醒来看见 loose end** | 重启后 scene re-warm,新 session 起来 | 按 seed 读 commitments.md + hot.md → 看到那条还没划掉的 open loop(以及 hot.md 里"答应了卡片但没见交付"的近期 episode)。首个 pulse 带"你刚回来(host 进程 Xm 前起的)" | 恢复靠 commitments + hot.md + journal,不靠老板重新交代;读记忆-醒来-注意是既有回路,不另起新机制 | 🟡 commitments.md 每个新 session 必读(`load_soul`)已是既有行为;"open loop / 重启自查"引导已发,未实测 |
| **4 · 先看已经落了什么** | — | 把这条 open loop 当成"重启打断的活":**重做前先看已经落了什么**——生词本里词已存好、views/ 里卡片建到哪了、有没有已经跟老板说过摆好了。数据已存就不再重新查词,只补没做完的渲染 | 别重复副作用:别重发消息、别重存文件、别重查已查过的 | 🟡 引导已发,未实测 |
| **5 · 判断还要不要,接着干 / 重新浮现** | — | 判断这件事还需不需要。这是老板等着的**面向用户的交付** → **主动出声轻量浮现**:"刚重启了一下,你之前要的那四张生词卡片我还没摆上来,这就给你",然后接着干完;干完**划掉** commitments 里那条。(若是纯自己的内部活,就悄悄干完划掉,不打扰老板。)| 面向用户的活出声重新浮现,内部活悄悄补;world 可能已经变了,所以是**重新判断**不是无脑重放;一句"还欠你 X,这就补"胜过既不出声硬磨、也胜过悄悄丢掉 | 🟡 引导已发,未实测 |

---

## Expected outcome

- 重启没把没干完的交付吞掉:老板要么看到卡片接着出现,要么收到一句轻量的"我还欠你 X,这就补"。
- **不需要老板重新交代**;也**不重复**已经做过的部分(词不重查、消息不重发、文件不重存)。
- 这条 open loop 干完后从 commitments.md 划掉,下次醒来不再被当成欠账。

## UX principles this journey establishes

- **半成品的交付是一条临时承诺**:接活当下就记进 commitments.md(和常驻职责同一个文件、同一套机制),交付即划掉。
- **恢复 = 读记忆-醒来-注意 这个既有回路**注意到 loose end → 判断还要不要 → 接着干;**不是** checkpoint 执行状态、也不是 resume 一个 ACP session。
- **能恢复的前提**:状态可从持久 journal + 落盘产物重建;只活在内存里的(worker)恢复不了——所以值得留的东西要落盘,恢复靠重建而非续命。
- **重做前先看已经落了什么**,别重复副作用。
- **面向用户的活出声重新浮现,内部活悄悄补**:都是同一套 pulse + commitments,区别只在要不要惊动老板。
- 这是 [03](03-feishu-flash-cards.md)/[02](02-feishu-sprint-backlog.md)/[05](05-news-and-watch.md) 常驻职责自愈的孪生:同一套机制,对象是一次性交付而非永久盯守。

## Edge cases & failure modes

- **重启时其实已经干完了**(只是没来得及划掉 commitments)→ 看产物 / 对话发现已交付 → 划掉,不重做、不重复浮现。
- **接活到记 commitments 之间就崩了**(jot-before-crash 窗口)→ reflection 兜底:它读 raw 看到"答应了没交付",在 episode 的 gist 里点明 → 投进 hot.md → 醒来照样看得到。单写者不冲突:reactor 写 commitments.md,代码(`refresh_hot`)写 hot.md。
- **老板重启后先开口说别的** → 先应老板;那条 open loop 别忘(读 commitments 时会看到),择机补上或顺带提一句。
- **open loop 已经过时 / 是别处的事** → 判断 still wanted,过时就划掉,别硬干;不无脑重放一个 world 已经变了的活。
- **重做会产生重复副作用**(已发过的消息、已存过的文件)→ 先查已落状态再动手,宁可少做也别重做。

## Open questions

- 老板在重启后**主动浮现**的节奏:多久内该浮现?好几条 open loop 时一次说完还是分开?
- 一条交付干到一半、产物只建了部分时,是从断点续(看 views/ 里建了多少)还是整件重做?目前交给 agent 判断"已经落了什么"。
- 临时承诺与常驻职责共用 commitments.md 会不会把文件搞乱?(都靠"交付即划掉"维持干净;若临时承诺太多需不需要分区?)

## 现状(2026-06-24 触发场景 → 2026-06-25 SHIPPED)

触发:scene `z2tysdcx` 老板让收四个生词做卡片,worker 正在做时 `make dev` 被 Ctrl-C 重启;worker 内存态丢失,卡片没摆出来;但生词本里词已存好(只丢了屏幕上的卡片渲染)。老板问能不能自己接着干完。

**SHIPPED 2026-06-25(57a757c),built+green on macmini,未实测。** 三处 soft-guidance 改动,不新增控制流,全程骑既有的"读记忆-醒来-注意"回路:

1. **core.md**「Your own operation」:reactor 接活当下把欠的交付记进 commitments.md(open loop),交付即划掉;重启后读到没划掉的 loop = 很可能是被打断的活 → 先看已落什么再重做 → 面向用户的出声浮现、内部的悄悄干完划掉。open loop 并入"首个 pulse 自查清单"。
2. **reflection.md**:jot-before-crash 兜底——segment 时把"答应了却没见交付"的事在 episode gist 里点明,经 `refresh_hot`(在 `reflect()` 末尾跑)投进 hot.md,醒来照样读得到;保持单写者。
3. **reactor/mod.rs**:首个 pulse note 改写为"你刚回来(host 进程 Xm 前起的)",保留 core.md 据以识别的事实。

### 待实测(复跑项)

按 [测试不要带witness](../../CLAUDE.md#testing-user-journeys-live-mac-mini) 的方法,在 Mac mini 上对真实实例跑:派一个有交付物的 worker → 中途杀进程 → 重启 → 看它是否读 commitments / hot.md、判断还要不要、不重复地接着干完(就用引发本特性的生词卡片场景)。当前全部 acts 标 🟡,等这次实测打分。
