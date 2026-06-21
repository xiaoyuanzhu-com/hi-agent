# 重复用到的 view,越用越快(完全相同必复用 / 同形换数据软引导)

**Persona:** 同一个用户在不同时间(同一对话内、隔天、换个 scene)反复要 agent 在屏上摆出"同一类东西";有时要的是上次那一份原物,有时要的是同一种样式换上新内容。
**Goal:** 像人用自己的工具箱——完全重复的**直接复用**(近零成本、画面一致),部分重复的**在旧件上改**,全新的才**从头做**;复用与否由"重复多少 / 新增多少"软性判断,目标是更快 / 更稳 / 更省,而不是死守规则。
**Preconditions:** view 工具箱会跨任务沉淀(`views/<project>/<name>.jsx` 持久存在);`show_view` 可按 **ref** 直接上屏(server 读盘 + 编译缓存命中,JSX 不进 mind 上下文);builder 动手前会先看工具箱(见 [appearance.md](../../src/reactor/appearance.md))。**与 [01](01-badminton-top10.md)(造前十演示)、[04](04-trending-feeds.md)(即时态现查)相连。**

---

同一句"羽毛球前十",可能是两种意思——"**再给我看上次那个**"(要同一份快照)与"**现在什么样了**"(要同一种、换最新数据)。前者走 Case A,后者走 Case B;这条分界决定复用还是改。

## Steps & expected UX

### Case A · 完全相同的 view 再次展示 → 必定复用

1. **第一次**:用户"我想看羽毛球男单前十" → agent delegate,builder 造好一组卡片、存成 ref(`badminton-top10/leader` …)、show 出来(完整流程见 [01](01-badminton-top10.md))。
2. **隔些时候再要同一份**:用户"再给我看下上次那个羽毛球前十" → agent **不再 delegate、不重查、不重造**,直接 `show_view(ref=...)` 把那组已存的 view 再摆出来。
3. **观感**:**明显更快**(几乎瞬时:server 读盘 + esbuild 缓存命中),且画面与上次**逐像素一致**(同一编译产物);agent 不重复自检、不重复口播研究过程。
4. **前提是 agent 能拿到那个 ref**;而"从哪拿到"随会话边界分三层(见下「复用怎么找到旧 view」):同一 session 里 ref 还在上下文,直接复用;跨 session 靠 reflection 把它沉淀出来;都没有时 builder `ls` 工具箱兜底。

### Case B · 同一种 view、内容是新的 → 软引导复用,按重复/新增比例决策

1. 用户"羽毛球前十**现在**什么样了" / "照上次那张卡的样子,换成**今天**的天气" → 要的是**同一种样式**、**新数据**。
2. builder 先看工具箱(`ls` + 读旧件),按**重复占比**决定怎么做(软引导,非硬规则):
   - **绝大部分照旧、只换少量数据**(排名结构没变,只换名次 / 数字 / 海报)→ 在旧件上**改字面量**,几乎不重画。
   - **结构相近、内容大改** → 以旧件为**起点**改写,省掉大半研究与设计脑力,house style 自动一致。
   - **跟已有的都不像** → 才**从头做**新的。
3. **观感**:比从零造**快**、且与同系列旧件**风格一致**;省下的是 builder 的研究 / 设计 / 自检脑力,不是上屏那一下。
4. **一个结构性事实(为什么 B 做不到 A 那样零成本)**:今天 view 是**静态**的——内容烤进源码,"换数据"= 换源码 = 新 ref + 重新编译 + 重新自检。所以 B 即便复用旧件,也至少要改源、重编译。要把"同形换数据"也降到接近 A 的成本,得让 view 能**参数化**(同一编译产物喂不同数据)——见 Open questions。

### 决策光谱(把 A/B 串成一条线)

| 重复 vs 新增 | 怎么做 | 成本 |
|---|---|---|
| 完全相同(同一份) | 直接 `show_view(ref)`,不 delegate | 近零(Case A,必复用) |
| 大部分旧 + 少量新数据 | 在旧件上改字面量 | 很低(Case B) |
| 结构相近 + 内容大改 | 以旧件为起点改写 | 中(Case B) |
| 几乎全新 | 从头 delegate 造 | 高(正常 build) |

### 复用怎么找到旧 view —— 三层(按会话边界递进)

| 层 | mind 怎么拿到 ref | 成本 / 时延 |
|---|---|---|
| **同一 reactor session** | ref 还在对话上下文里(刚委托造完、刚 show 过)| 即时;只需一句软引导——重造前先看本会话是否已建过 |
| **跨 session** | reflection 把重复的 view 沉淀成 handle,经 `hot.md` 常驻载入 | 有 reflection 时延(可接受,非实时场景)|
| **冷兜底** | builder `ls` 工具箱、按主题找回 | 慢,但总能成 |

**沉淀落到哪**:view 本体(`.jsx` 源)始终留在工具箱 `views/<project>/<name>.jsx`,reflection **不搬它**;沉淀的只是一条 **purpose→ref 的 handle**,作为一个 facet 落在 `memory/facets/views/<subject>/facet.md`(facet 维度本就开放、非枚举),像别的 facet 一样**由 episodes 重生成、claim 带出处**(造它 / show 它的 episode);最热的若干条投影进 `memory/hot.md` 常驻,让"我已经有了 → `show_view(ref)`"在上下文里直接触发。这一整套沿用现成的记忆梯度(raw → episodes → facets → hot.md),不另起炉灶——一个反复有用的 view,就是 agent 理解的又一个 subject。

## Expected outcome

- 同一类东西**越用越快**:第二次要"同一份"近乎瞬时;要"同一种换数据"也明显比第一次省事。
- 复用自带**一致性红利**:同系列 view 风格统一,无需额外对齐。
- agent **不重复劳动**:不把已经造好的东西再造一遍,不重复研究 / 自检。
- 复用是**判断**出来的:builder 按重复 / 新增比例在"直接复用 / 改 / 从头"之间软性取舍,不死守规则。

## UX principles this journey establishes

- **重复用到的 view 越用越快**;复用是积累出来的,不预置组件库([[no-prebundled-assets-accumulate-via-guidance]])——我们让积累的工具箱**好找、好复用**,而不是出厂塞一套。
- **完全相同 → 必定复用**:直接按 ref 上屏,不重造、不重查、不重自检,画面与上次一致;前提是 mind 能**发现**已有的 ref。
- **同形换数据 → 软引导复用**:按"重复多少 / 新增多少"在改旧件 / 以旧为起点 / 从头做之间判断;给软引导,不强制。
- **一致性是免费的**:从旧件出发自动保持 house style。
- **自己的工具箱自己管**:view 按"它是什么"命名(非按今天的任务),日后按主题找得回——这是复用的前提。

## Edge cases & failure modes

- **旧件过时 / 坏了** → 不盲目复用;该改就改、该弃就弃(对工具箱里的旧件也要"用前核实",别拿着旧 ref 就上)。
- **误判成相同**(内容其实已变却直接 ref 上屏)→ 画面驴唇不对马嘴;发现目录要给够区分信息(每条一行用途),避免撞名复用错。
- **找不到本该有的旧件**(命名 / 目录乱)→ 退化为重造;命名规范是复用能成立的前提。
- **跨 scene / session 复用**:ref 在 views tree 里持久、全局存在,复用不应只限于"当前对话还记得的 ref"——这正是需要 mind 可见发现目录的地方。
- **用户其实要"最新"**:把 Case B 误当 Case A(给了旧快照)→ 该现查 + 换数据(连 [04](04-trending-feeds.md));要区分"再看上次那份" vs "现在怎样"。

## Open questions

- **跨 session 复用依赖 hot.md 策展(当前 deferred)**:把 handle 沉成 `facets/views/` 已贴合现有 facet 机制([[project_memory_subsystem_redesign]]),但"必复用"要它常驻 `hot.md` 才在上下文里触发;hot.md 的常驻策展尚未建,这条链依赖它先落地。**in-session 那层不依赖它**——只差一句软引导。
- **要不要做参数化 view**:给 `show_view` 加一条 data 通道 → 同一编译产物喂不同数据,把 Case B 的"同形换数据"降到接近零成本。代价:引入数据面、偏离现在"内容烤进源码、JSX 不进 mind 上下文"的静态模型——是一次有意的架构取舍,不是免费午餐。
- **软引导给到多细**:"重复占比"到什么程度该改 vs 从头,全交给 builder 判断够不够?要不要一句粗略指引?
- **三者权衡**:复用(快 / 一致) vs 新鲜(对得上当前数据) vs 从头(最贴合)——有没有需要明示的优先级?

_机制:**同一 session 复用**零件已在(`show_view` by ref + 内容寻址编译缓存),只差一句软引导;**跨 session** 靠 reflection 把 handle 沉成 `facets/views/` 并投影进 `hot.md`(贴合现成 facet 梯度,但 **hot.md 策展未建**);**冷兜底**靠 builder `ls` 工具箱([appearance.md](../../src/reactor/appearance.md) 现有 guidance)。Case B 软引导同样靠 appearance.md,受限于 view **静态、换数据必重编译**(参数化 view 见 Open questions)。成熟度:**in-session 与 ls 兜底部分具备、reflection 沉淀 / hot.md / 参数化 view 未建**。_

## 实测 2026-06-21 · origin/main efb228e(boss 文字通道 + 隔离实例驱动)

环境:Mac mini,用 `--data-dir` 起**独立空目录的隔离实例**(避开旧 scene 污染),挂 `/api/out/view` 长轮询当"屏幕在场"。三幕:① 做"本周三目标"卡片 → ② 做"下周目标"卡片 → 清屏 → ③ 让它把第一张再放出来。Ground truth 取自 reactor transcript 的 `tool_use`、`channel out (view)` 事件、ACP spawn 时间线、views 树、`facets/` 与 `hot.md`。

- ✅ **同一 session 复用成立(Case A 核心,印证用户诉求)**:第三幕"把本周目标那张再放出来",reactor 只发了**一个** `show_view{ref:"weekly-goals/card"}`——**不 delegate、不起 worker、不重写**;屏上重现的是**同一个编译产物**(`b4602c0a…mjs`,内容寻址命中),清屏→重现**约 7 秒**(对比首建 6.5 分钟)。ref 全程在 reactor 的对话上下文里,正是"in-session 那层只需把已有 ref 再 show 一次"。
- ✅ **同形换数据→改而非从头(Case B 软引导)**:第二幕做"下周目标"卡时,reactor 的 delegate brief 自己写了"**in the same style as the weekly goals card you just made (weekly-goals/card)**";产物 `next-card.jsx` 与 `card.jsx` 一 diff 就是**在旧件上改**(结构照搬,只换标题/日期/条目、glow 微调),不是冷起。"看工具箱、能改别重画"在真实运行里发生了。
- ✅ **build 成本主要在首建**:首建 6.5 分钟几乎全花在 builder 自检的**首跑 headless 浏览器置备**;第二张同类卡 ~83 秒(浏览器已缓存 + worker 复用,见下),复用 ~7 秒。"重复越用越快"在数量级上成立。
- 📝 **worker 进程也被复用**:两次 delegate 只在首建时 spawn 了 **1 个 worker**;第二幕 delegate **无新 spawn**——首个 worker 保持温热被复用。属进程级复用,与 view 复用正交,但同样省时。
- ⚠️ **跨 session 沉淀这层确认未建(符合预期)**:跑完 `memory/facets/` 只有 `projects/hi-agent`,**没有 `views/` 维度**;`hot.md` 被 reflection 重写了,但写的是**情景记忆**("老板让我做了张卡片…"),**不是 purpose→ref 的复用 handle**——未来 session 据此**无法**直接 `show_view(ref)`。reflection 子系统本身**活着**(本次自适应时钟触发 2 次),即三层里的"宿主"在跑,只是还不沉淀 view handle。这是对 journey 待建项(`facets/views/` + hot.md 策展)的实测确认。
- ⚠️ **同一 session 内 view 默认叠加、不替换**:第二幕新卡用 `op:show` 加新 id(`weekly-goals-next`),旧卡仍留屏上(v2 两个 module 并存),要单独看回第一张得先清屏。非本 journey 缺口,但说明"换一张"默认是加视图、由 reactor 用 id/dismiss 自己管布局。

复核:**in-session 复用 + Case B 改写 = 实测通过**;**跨 session 沉淀 / hot.md 复用 handle / 参数化 view = 未建(facets/hot.md 实测佐证)**。与正文成熟度标注一致,无需翻案。
