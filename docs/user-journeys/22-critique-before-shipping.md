# 剪完先自己看一遍(批判反射:好看,不只是能用)

**Persona:** 同一段集锦活,镜头对准**交付前的自检**。普通用户在等成片。
**Goal:** worker 把东西交回、agent 把东西上屏**之前**,先**冷眼**看一遍成品——对着现查来的好范例比一比:这是"能用"还是"真好看"?不到位就再来一版,**过线即止**,然后才出手。
**Preconditions:** 有范例当标尺(连 [21](21-research-before-stale-answer.md) 现查的范例);worker 能真看自己的产物(看片 / 读文件,不是"命令成功了"就算)。**core.md 已有种子:"东西离手前先看它本身;'命令成功'不是'结果对'"([core.md](../../src/reactor/core.md))——本条把它从「对不对」推到「好不好」。** 与 [03](03-feishu-flash-cards.md)(样稿校准的翻车与重做)、[12](12-play-with-child.md)(够精致才上 views/)相连。

---

研究反射(21)管的是**开工前**别拿陈货;批判反射管的是**收工前**别把 dumb 的交出去。一刀切出来的"能放"初稿——节奏拖、精彩点平、转场土——技术上**成功**了,却**不好看**。差别就在有没有人**冷眼**再看一遍。当前 worker prompt 偏"make the reasonable assumption, keep going, work to completion"([workers.rs](../../src/reactor/workers.rs)),那是冲着**做完**去的,不是**做好**。

## Steps & expected UX

1. **worker 剪出第一版** → **不直接交**。先当观众看一遍:节奏拖不拖、精彩点够不够、转场土不土、跟现查的好范例差在哪。
2. **发现"能放但平、像机器切的"** → **自己判翻车,重剪**(收紧节奏、换更好的精彩点、调转场)。worker 有的是时间——它就是吸收静默的那个,多一版好过交一版 dumb。
3. **过线** → 才交回 + 报告(连 [03](03-feishu-flash-cards.md) 的"交付必检")。**过线即止,不无限磨。**
4. **agent 收到的是已经过自检的成片**,口播 gist + 上屏。

## Expected outcome

- 用户看到的第一版就**对得上那条 bar**,而不是"能用但土"的初稿等着用户来骂(对比 [03](03-feishu-flash-cards.md):用户当场翻车后才重做)。worker 自己把 dumb 的拦下了。
- 失败 / 跳过的如实说(连 [speaking.md](../../src/reactor/speaking.md) 收尾)——咽下去的瑕疵比慢一点更糟。

## UX principles this journey establishes

- **交付前冷眼自评,对着好范例打分。** 不是"跑通了吗",是"好看吗、对得上那几个好例子吗"。研究反射在**开工前**铺下范例,批判反射在**收工前**拿它当尺——同一条线的两头(连 [21](21-research-before-stale-answer.md))。
- **"能用"不等于"好"。** 把 core.md 现有的**对不对**自检("succeeded≠right")推到**好不好**;这一步直接堵掉"works but dumb"。
- **过线即止,不是封顶。** 在 worker"做完就走"的偏向前加一道"做好了吗"的闸——但闸是**过线**(够好就停),不是**无限打磨**。

## Edge cases & failure modes

- **没有范例当标尺**(21 没查 / 查不到)→ 退而用 durable 审美 + 诚实标"没现看当下范例";批判反射不靠外部范例也该有基本的"这平不平"判断。
- **无限打磨**(为完美卡住不交)→ 与 worker"work to completion"是另一极端;批判反射要的是**过线**就停,不是磨到完美;给个粗略上限(见 Open questions)。
- **用户赶时间** → 降批判强度(基本不翻车即可),别为打磨拖了交付。
- **自评走过场**(看了等于没看,照样交 dumb)→ 自评得对着**具体标尺**(范例 + aesthetic.md 那类 bar),不是空泛"看一眼";没有尺,自评就退化成盖章。

## Open questions

- **"bar"谁定?** 现查范例当标尺 + 把 [aesthetic.md](../../src/reactor/aesthetic.md)(views 已有的 bar)推广到非-view 产物(视频 / 文档 / 图)?
- **自评到几版收手?** 给个粗略上限(一两版)还是全交给 worker 判断?怎么避免既不 dumb 也不无限磨。
- **与 21 的耦合**:范例既是开工前的参照、又是收工前的标尺——要不要在 guidance 里把这条线点明,让 worker 把"开工查的范例"留到收工当尺用?

_机制:现有种子——core.md "离手前先看 / succeeded≠right"、[03](03-feishu-flash-cards.md) 交付必检、[aesthetic.md](../../src/reactor/aesthetic.md) 给 views 的 bar。本 journey 把"对不对"推到"好不好",主体在 worker prompt + core.md 加 guidance(对着范例自评 + 过线即止)。成熟度:**correctness 自检的种子在,quality 批判反射未写**;worker prompt 现偏"work to completion",需补一道"好不好"的过线闸。_
