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
- [02 · 飞书群消息 → Sprint-Backlog 任务](02-feishu-sprint-backlog.md) — 常驻委托,从零开始:置备工具(装 CLI、建应用、鉴权,老板只做必须的事)→ 对齐 → 试运行 → 长期值守;确立缺工具是任务的一部分、向上沟通最小化、heartbeat 自我注意、重启自恢复等原则。
- [03 · 飞书群 flash-cards → 记忆卡片图](03-feishu-flash-cards.md) — **按真实运行写成的完整实例**:委托 → CLI 置备与三次扫码 → 样稿校准(翻车与重做)→ 补齐存量 → 断后自愈;确立交付必检、长活不占线、完工交差、自己的摊子自己管;附实测缺口清单。
- [04 · 看 GitHub/小红书/抖音在火什么](04-trending-feeds.md) — 即时世界态·按需现查(扩展 01);内容不入持久记忆,只有站定兴趣才落 facet。
- [05 · 今天有什么大新闻 / 盯住油价](05-news-and-watch.md) — 一次性现查 vs "盯着"落成长期关注 + pulse 主动浮现;重启不丢盯。
- [06 · 我附近的羽毛球活动](06-badminton-near-me.md) — 01 的延伸:用站定兴趣(羽毛球)+ 位置过滤 + 主动浮现一句,可叫停。
- [07 · 用浏览器替我办事](07-browser-errand.md) — 浏览器 effector 实操(非纸上谈兵);敏感动作停下请示;怎么开页沉淀成技能。
- [08 · 操作电脑/手机上的应用](08-operate-apps.md) — Mac/Win/Linux/Android/iOS 的可行性光谱;有句柄才做,没有就诚实说清。
- [09 · 用微信(诚实面对脆弱面)](09-wechat.md) — 个人号无开放 API + 反自动化;给受限路径,不假装与飞书同等。
- [10 · 用 SAM/YOLO 做视觉活](10-vision-sam-yolo.md) — 真跑出检测/掩膜;首选不灵主动换方案(连 14)。
- [11 · 在中国报个税](11-china-tax.md) — 半稳定领域:用前现查当年政策/数字;截止日可作主动提醒。
- [12 · 陪孩子:讲故事/教认数字/做图](12-play-with-child.md) — register 适配 + 适龄 + 安全边界;够精致的图走 views/。
- [13 · 配一个外部能力(API + 凭证)](13-equip-a-capability.md) — 能力分流:认识进记忆/技能,凭证逐字进 drive 笔记本,密钥不进脑子。
- [14 · 你对 YOLO 的了解随用而长、被实践修正](14-knowledge-grows.md) — competence 读自证据图(不存等级)+ provenance + 先验被 lived 超越;验证知识模型的核心 journey。
- [15 · 打断:我一开口,它就让路](15-talk-over-the-agent.md) — 语音对话的底座:嘴串行、字跟嘴走不抢跑;我插话它当场停声并清掉没说出口的尾巴,下一轮带"说到哪 / 我说了啥"重新组织,不复读。
- [16 · 先认得脸/声音,后来才知道名字,然后处处用得上](16-recognize-people.md) — 身份=生物特征簇:不报名字也能先把人记成"同一个人"(mint 一个 id),名字从对话里学到后把 id 改名成名字;认人是软证据、容忍模糊、可纠错。脸已端到端实测(buffalo_l),声音待建。
- [17 · 播放音乐(开 app → 搜放 → 投屏 → 记住偏好)](17-play-music.md) — 随口点歌:有 app 就用、没有先请示装;搜到真放出来;播放界面投成 view,可收起转后台(收画面≠停播);第二次记住用哪个 app、怎么搜放、投屏与否,更省事。
- [18 · 我要传你点东西,怎么弄(摆出上传入口:拖拽区 + 二维码)](18-send-files-to-agent.md) — 最基础常用的一步:把东西递给它。优先**直接摆两个 view**(拖拽区 + 手机扫码上传页)而非口述选项;入口绑 scene。文件 = 递来的物件按引用,不走 vision 感官。行为靠 show_view 已有,carrier(上传端点 + 手机页 + 二维码)未建。
- [19 · 直接传一张护照照片(收下 → 看懂 → 存进 drive → 妥帖回话)](19-upload-passport.md) — 把文件当**物件**收下:原件逐字进 drive、认识带出处进脑子(不是当"看到一幅画"配字幕);看懂是什么/属于谁,妥帖回话,敏感件确认意图、默认私密;日后"我要护照"找得回。carrier/drive/解析未建。
- [20 · 重复用到的 view 越用越快(完全相同必复用 / 同形换数据软引导)](20-reuse-built-views.md) — 像人用工具箱:完全相同→直接 `show_view(ref)` 必复用(近零成本、画面一致);同形换数据→builder 看工具箱按重复/新增比例软引导(改旧件 / 以旧为起点 / 从头)。复用分三层:in-session 上下文里、跨 session 靠 reflection 沉成 `facets/views/`→`hot.md`、builder `ls` 兜底;hot.md 策展与参数化 view 待建。
