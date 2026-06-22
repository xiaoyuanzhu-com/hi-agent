# 第二次剪快得多,而且没用陈货(技能沉淀:一次难活变顺手的流程,且会重新核当下)

**Persona:** 同一用户,隔些时候又递一段比赛"再剪个集锦";这中间 agent 真剪过一次。
**Goal:** 第一次又查又试又翻车的"贵"经历,**沉成一条技能**(怎么剪、用什么、坑在哪、什么样算好);第二次**从那条线起步**,明显快、起点就在 bar 上——但技能里**会过期的那半**(当下剪法 / 工具新版本)第二次**重新核**,不把旧的当真理固化。
**Preconditions:** 有个 `skills/` 工坊(挨着 `views/`),技能是 agent 自己话写的笔记;reflection 会把一次成功的难活策展成干净可复用的笔记。**复用现成模式:[20](20-reuse-built-views.md)(view 复用的三层:in-session / 跨 session 靠 reflection 沉淀 / builder `ls` 兜底)、self.md(自己话写的常驻笔记)、reflection 给 drive 做 housekeeping 的先例(d4af1be)。与 [14](14-knowledge-grows.md)(懂得随用而长)、[11](11-china-tax.md)(技能怎么带"重新核当年规则")相连——本条正面回答 11 的 open question。**

---

人把一次难活变成顺手的流程:第一次剪集锦很贵(研究当下剪法、试、翻车、重来),第二次就有了套路、有了模板、有了"这客户爱快切"的品味。**agent 今天把这次难活只记成一段「经历」(episode),没记成一条「我会怎么干」(skill)**——于是每次从零摸起。20 让 view 这一类沉淀下来复用;本条把它推广到**技能**,并补上一个关键 nuance:技能不是冻结的真理。

## Steps & expected UX

1. **第二次"再剪个集锦"** → agent 动手前先**翻工坊**:这活我干过吗?有,**起点拿来**(连 [20](20-reuse-built-views.md) 的"看工具箱再动手")。
2. **worker 从技能笔记起步**:流程 / 工具 / 坑直接复用,**不从头摸**;但笔记里标着"剪法 / 配乐是会过期的" → 这半**重新现查**(连 [21](21-research-before-stale-answer.md)),其余照旧。
3. **观感**:明显更快(省掉首次的研究 + 试错 + 翻车),且起点就在 bar 上(批判的标尺一并记着,连 [22](22-critique-before-shipping.md));该重核的重核了,不是拿几年前的剪法硬套。
4. **(沉淀这步本身)** 第一次干完、reflection 时:把"剪集锦"这件**难 + 会再来 + 干成了**的事,策展成 `skills/` 下一条干净笔记;重复的合并,过时的(依赖的工具变了)修剪——就像它已经在给 facets / drive 做的策展。

## Expected outcome

- 同类难活**越干越快**:第二次从 bar 起步而非从零。
- **不固化陈货**:技能带着"重新核当下"的自检——durable 半(我怎么剪)稳着用,transient 半(当下什么算好)每次被研究反射重核。**这正面回答 [11](11-china-tax.md) 的 open question**:报税技能不会把旧数字焊死,它存的是"去哪查当年口径",不是"今年扣 5000"。
- **不是每件事都沉淀**(只沉淀难 + 会再来 + 干成了的),免得工坊堆垃圾。

## UX principles this journey establishes

- **一次难活 → 一条可复用技能**;门槛是判断:**难 + 会再来 + 干成了**才存,不是每活都存。
- **技能是起点,不是真理。** 它装着两半:durable 的"我怎么干"稳着用,transient 的"当下什么好 / 哪个工具"**每次被研究反射(21)重核**。技能 = 把一次研究+批判的贵成本**结晶**成起点,不是省掉再看的**替代**。
- **动手前先翻工坊**(连 [20](20-reuse-built-views.md)):干过的别从头摸。
- **reflection 策展技能**:跟它策展 facts(facets)、收拾 drive(d4af1be)同一趟手艺——promote 一次性成功成干净笔记、合并重复、修剪过时。
- **自己攒的东西自己管**:技能按"它是什么"命名(非按今天的任务),日后按主题找得回——复用的前提(连 [20](20-reuse-built-views.md))。

## Edge cases & failure modes

- **技能过时**(依赖的工具 / 剪法变了)→ 不盲用;研究反射重核 transient 半,reflection 修剪烂笔记(连 [20](20-reuse-built-views.md) "旧件用前核实")。
- **沉淀过度**(每件小事都存)→ 工坊堆垃圾、难找;守住门槛=难 + 会再来 + 干成了。
- **找不到本该有的技能**(命名 / 目录乱)→ 退化为重新摸;命名按"它是什么"。
- **把 transient 半也当真理固化**(只复用不重核)→ 正是 [11](11-china-tax.md) 怕的"固化旧数字";技能笔记里**显式标**哪半会过期,让研究反射知道该重核哪。

## Open questions

- **`skills/` 笔记的结构**:纯自由话(像 self.md)还是带个轻约定标出"哪部分会过期"?——本条主张至少要能让 21 认出 transient 半,所以倾向一个极轻的"会过期"标记。
- **reflection 策展技能 vs 策展 facts** 是同一趟还是分开?门槛怎么定(连 [14](14-knowledge-grows.md) 的 competence:懂多少读自证据图)?
- **统一在记忆梯度里讲?** 技能、view 工具箱([20](20-reuse-built-views.md))、drive(d4af1be)同属"agent 自己攒的东西"——要不要都顺着 raw→episodes→facets→hot.md 讲,而非另起炉灶?
- **capability-gap**("我连个剪辑工具都没有 → 装 / 建 / 问")并到这条线,还是另起(连 [13](13-equip-a-capability.md))?本条假设工具已在,只沉淀**怎么用**。

_机制:复用现成模式——`skills/` 工坊照 [views/](20-reuse-built-views.md) + self.md;reflection 策展照它给 facets / drive(d4af1be)的策展;reuse-before-start 照 [appearance.md](../../src/reactor/appearance.md)。新结构只有 `skills/` 工坊 + reflection 多策展一类。本 journey 的**核心主张**——技能与研究反射(21)的"重核 transient 半"耦合——是把"越用越快"和"不固化陈货"两件事拧成一股,正面解 [11](11-china-tax.md) 的悬案。成熟度:**模式都在,`skills/` 工坊与 reflection 策展技能未建;21↔23 的重核耦合未实现。**_
