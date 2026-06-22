# 把一坨数据交给它(Apple Health 导出 / Claude Code 会话)—— 先落 raw,值得留的再沉进 drive

**Persona:** 用户手里有**一批**数据要交给 agent——一份 Apple Health 导出(几百 MB 的 XML/zip,量化时序),或一沓 Claude Code 项目会话(一堆 `.jsonl` 工作记录)。不是一张护照那样的单个物件([19](19-upload-passport.md)),是一**坨**、且**有结构**。
**Goal:** agent 别把它当 ETL"导入"(它不是数据仓库,也没有"导入"这个动作);而是像人收到一摞材料——先**收下并就地留住**(落 raw 那刻即 precious),看懂个大概,**值得长期留的逐字沉进 drive、能理解的化进记忆**;且分清"我现在就要存好"与"用着发现值得留"两扇门。
**Preconditions:** 有接收入口([18](18-send-files-to-agent.md));`raw/` 已是 append-only、precious+synced 的真相层(memory.md §3);理解 `drive/` 与 graduation、save-to-drive 引导([data-dir-layout](../data-dir-layout.md))、reflection 沉淀([20](20-reuse-built-views.md))。**与 [19](19-upload-passport.md)(单个物件)对照,这条是"一坨 / 结构化";底层模型见 [[file-exchange-drive-carriers]] 与 [[project_memory_subsystem_redesign]]。**

---

**没有"导入"这个动词。** hi-agent 围绕「持续感知 → reflection 消化 → 刻意留存」建,不是 ETL / 数据仓库。所以"把这批数据导进来"不会触发一条 schema 导入管线;它落成一次 agentic 处理:收下、就地留住、看懂、**选择性**沉淀。下面先讲两类数据**共同**的生命周期(先落 raw,再选择性毕业进 drive),再讲两类数据**各自**怎么映射(工作记录 vs 量化时序),最后是大宗字节那个结构性坑。

## Steps & expected UX

### 共同的生命周期 · 先落 raw → (选择性)毕业进 drive

1. **交上来** → agent 收到的是"老板递来一坨 X"**这件事**:落一条 raw signal(text surface:"收到 `apple-health-export.zip`,480MB" / "收到 37 个 Claude Code 会话"),**字节就地留住**。落 raw 那刻起它就 precious+synced,**不会丢**——后面毕不毕业是"组织 / 留存",不是"安全"。
   - **反例(别这么干)**:别真去跑一遍 schema 把它塞进某张表——hi-agent 没有这个动作,也不该假装有。
2. **看懂个大概** → 这是什么、有多大、值不值得长期留;不必当场细嚼。
3. **两扇门进 drive**(关键分界):
   - **明确"存下来"**(老板说"把这些存好")→ **当场**由 live mind 委托 worker 把原件逐字存进 drive、写一条带出处的记忆指针。低时延,刚开口就办(范型见 [19](19-upload-passport.md))。
   - **用着发现值得留** → 不急的交给 reflection,像 view 那样**毕业**:把原件从暂存沉进 drive、写一条 purpose→path 的 claim(沿用 [20](20-reuse-built-views.md) 的 graduation;"值得留"本就是 consolidation 判断,正是 reflection 的活)。
4. **digest 与 keep-verbatim 同时发生** → **意义化进 facets**(模糊、可成长的理解),**确切字节逐字进 drive**(查得回的原物)。一份数据两者都要:留住字节,也形成理解。
   - **红线**:reflection 进 drive 只**逐字归档 + 写指针**,**绝不 paraphrase** drive 里的字节——这正是 data-dir-layout 决策表"`drive/` verbatim、reflection-read-only"在保护的(read-only = 不改写 / 不消化已存字节,而非"不能新归档")。

### Case A · Claude Code 会话 —— 工作记录,贴近现成 episodes/facets

1. 会话是对话 / 工作记录,**接近 hi-agent 记忆本来就建模的东西**。看懂"这些是哪个项目、做过什么、踩过什么坑",化成 episodes / facets("X 项目那阵在搞鉴权重构""我在 Rust 上反复用过 Y")。
2. 原件逐字留 drive 备查,理解进脑子(带出处,连 [14](14-knowledge-grows.md) 的 provenance:"我读到的会话里…")。
3. **观感**:日后问"我上次在 X 上是怎么解决的",答得出,且能追溯到具体会话。

### Case B · Apple Health 导出 —— 量化时序,不是给重构记忆的料

1. 健康导出是**量化时间序列**;hi-agent 的记忆是 **lived-signal 的重构记忆**,有意**不是**可查询数值库——你不会想从 facet 里查"近 90 天 HRV 曲线"。
2. 所以分开走:**原件逐字留 drive**(确切字节);**要看趋势 / 出图是分析活**,交给独立的 `apple-health` skill(分析工具,与 hi-agent runtime 正交);只把**结论性理解**化进记忆("三月静息心率走低")。
3. **反例(别这么干)**:别想把 480MB 数值"消化"成 facets——既丢精度又塞爆记忆。数值该 drive 逐字留 + 按需分析,不是嚼成模糊印象。

### 一个结构性坑 · 大宗字节别"穿过 raw 再复制到 drive"

- 单个护照([19](19-upload-passport.md))直接落 `raw/files` 很干净。但一份 480MB 导出不一样:`raw/` 是 precious+synced,且**数据集不属于会褪色的 media**(褪色是给 mic / camera 分钟的 vividness 的)。把大字节先塞 raw、再**复制**进同样 synced 的 drive,是**纯重复、零 durability 收益**——raw 那份本就逐字永久,搁着就是死重。
- 所以大宗数据:raw 持 **event / pointer**(那条"收到 X"的 signal,bookkeeping 不是 speech),字节 **move / stage 而非复制穿两棵 synced 树**。毕业在这里是"提升为有组织、全局可寻址的留存",不是安全副本。

## Expected outcome

- **不被当 ETL**:agent 收下、就地留住、看懂、选择性沉淀,而不是跑一个并不存在的导入管线。
- **一切先落 raw**(落那刻即 precious,不丢);值得留的**逐字进 drive**、能理解的**化进 facets**。
- **两扇门各就各位**:明确"存好"→ 当场办(live → worker);"发现值得留"→ reflection 毕业。
- **两类数据各得其所**:工作记录化进 episodes / facets;量化时序留 drive 逐字 + 交分析工具,不硬塞记忆。
- **大宗字节不在两棵 synced 树间重复**。

## UX principles this journey establishes

- **没有"导入"这个动作**:大宗 / 结构化数据同样是 agentic 的收 + 留 + 消化,不是 schema ETL。
- **先落 raw,再毕业**:raw 是真相、落即 precious;进 drive 是"逐字留住"的刻意之举,不是入库的副作用。
- **两扇门**:明确存 = live 当场办(刚开口、要低时延);发现值得留 = reflection 毕业(沿 [20](20-reuse-built-views.md))。
- **意义化进记忆、字节逐字进 drive**:数值 / 原件别消化成模糊理解,理解也别去背确切字节。
- **reflection 只逐字归档、绝不 paraphrase drive**:这是"逐字附件"这件事能成立的红线。
- **该外部分析的就别塞进脑子**:可查询的量化分析是 skill / view 的活,不是记忆的活。

## Edge cases & failure modes

- **其实要的是可查询数值库**(经常做健康分析)→ 那不是记忆该干的;留 drive 逐字 + 用 `apple-health` skill 或一个专门 view,别逼记忆当数据库。
- **数据集太大、穿不动 raw** → raw 落 pointer,字节 stage / move 到它要住的地方;别为复制而复制(见上面那个坑)。
- **增量再导一次**(又一批会话 / 下个月健康数据)→ 今天**没有去重 / 增量同步**;别假装有,如实说"这批我收了",理解可合、原件各留各的。
- **隐私敏感**(健康数据 / 工作会话里夹着密钥)→ 默认私密:别投公共 view、别外发、密钥不进脑子(连 [13](13-equip-a-capability.md) / [19](19-upload-passport.md))。
- **混淆"导入到 hi-agent"与"归档进我的生活库"** → 两回事:hi-agent 的 `drive/` 是它**自己的脑子 / 电脑**,人类的 my-life-db 是你自己的库(原样保存、别重构)。别把 agent 的记忆当成你生活库的镜像;它消化出的是**它的理解**,不是你的归档。

## Open questions

- **大宗结构化数据到底归哪**(本 journey 暴露的核心设计接缝):(a) 消化进重构记忆、(b) 逐字进 drive 按需查、(c) 留在 my-life-db 让 agent 读——三者边界未定。倾向:意义→记忆、字节→drive、可查询数值库→外部 skill / view;但**没定**。
- **reflection 毕业 vs live 当场存的分界**:全凭"明确 vs 涌现"判断够不够?要不要一句指引(像 [20](20-reuse-built-views.md) 的软引导)?
- **raw 暂存大字节的形态**:move 还是 stage、落到哪?`raw/files` 当 scene-bound 暂存、drive 当有组织的家,这个分工对不对?
- **增量 / 去重**:再导一次怎么合(理解可合、原件各留),今天都没有。
- **分析 skill 与 drive 原件怎么搭**:`apple-health` 这类 skill 读 drive 里的原件出图,结论回流记忆?谁触发、多久一次?

_机制:接收靠 carrier([18](18-send-files-to-agent.md),web upload 已定为内置 seed);落 raw 靠现成 journal(event / pointer 作 signal,字节就地);进 drive 两扇门——明确存 = `core.md` 判→委托 worker(save-to-drive 引导已写、未实测),涌现留 = reflection 毕业(沿 [20](20-reuse-built-views.md) 的沉淀,贴现成 raw→episodes→facets 梯度);意义化进 facets 是现成 reflection;量化分析走独立 `apple-health` skill,与 runtime 正交。可行性:可行,依赖 drive / files / graduation(均设计未建,见 [data-dir-layout](../data-dir-layout.md) 状态)。成熟度:**raw 落地具备;drive / 毕业 / save-to-drive 引导 / bulk-字节 move 语义 / 增量合并 均未建或仅引导未实测。**_
