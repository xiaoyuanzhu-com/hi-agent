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
- [10 · 用 SAM/YOLO 做视觉活](10-vision-sam-yolo.md) — **测的是"能建项目 + 接上 appearance"**(视觉任务只是载体,非通用视觉感官):真跑出检测/掩膜并落 `views/` 呈现;首选不灵主动换方案(连 14)。
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
- [21 · 把一坨数据交给它(Apple Health 导出 / Claude Code 会话)](21-hand-over-bulk-data.md) — 大宗/结构化数据不是 ETL"导入":先落 raw(落即 precious、不丢),值得留的逐字进 drive、能理解的化进 facets。两扇门:明确"存好"→ live 当场委托 worker;"发现值得留"→ reflection 像 view 那样毕业。工作记录贴近 episodes/facets;量化时序留 drive + 交独立 apple-health skill,不硬塞记忆。坑:大字节别穿 raw 再复制进 drive(两棵都 synced)。drive/毕业/增量合并待建。

> **像人一样攒能力的三条反射(22–24)**:不是塞一个大知识库,而是按 decay-rate 配上**获取反射** + 元心态。研究反射(开工前别拿陈货)→ 批判反射(收工前别交 dumb)→ 技能沉淀(把贵经历存成起点,且会重新核当下)。研究/批判主体是 soft-guidance;技能沉淀要一个 `skills/` 工坊 + reflection 多策展一类。

- [22 · 给我剪个集锦(研究反射:别拿陈货当数,先去看)](22-research-before-stale-answer.md) — 把"现查"从用户明说的易例([04](04-trending-feeds.md)/[11](11-china-tax.md))推到 agent **自以为知道**的难例。触发是个**味道**:要给"best/latest/现在/哪个工具/什么流行"的判断时——"我不是**知道**、是**记得**"→去看(含**看范例**校准品味,不只查事实);限定**快过期层**(durable 手艺不查),否则每件小事都查会卡。能力本就有(worker 有 web+code-exec),缺的是**反射触发**——三条里最便宜、该先落。**已实测通过**(它现查 Kokoro 而非背旧榜)。
- [23 · 剪完先自己看一遍(批判反射:好看,不只是能用)](23-critique-before-shipping.md) — 交付前**冷眼自评**,对着研究反射现查来的好范例打分:"能用"≠"好",不到位再来一版、**过线即止**(非无限磨)。把 core.md 的对不对自检(succeeded≠right)推到**好不好**,直接堵 works-but-dumb;在 worker 现有"work to completion"偏向前加一道好不好闸。实测:worker 渲染成图、看实际成品再迭代;审美过线闸未单独隔离。
- [24 · 第二次剪快得多,而且没用陈货(技能沉淀:难活变顺手流程,且重新核当下)](24-skill-improves-and-refreshes.md) — 一次又查又试又翻车的贵经历沉成 `skills/` 一条笔记,第二次**从 bar 起步**、明显快;但**技能=起点非真理**:durable 半(我怎么干)稳用、transient 半(当下什么好/哪个工具)每次被研究反射([22](22-research-before-stale-answer.md))**重核**——正面解 [11](11-china-tax.md) 的"技能别把旧数字焊死"。门槛=难+会再来+干成了;reflection 策展(照它策展 facets/drive)。实测:worker 自己写了一条技能笔记(contribute 路径通过);reuse/策展/transient 标记待复测。
- [25 · 干到一半被打断,重启后自己接着干完(一次性交付的断点恢复)](25-resume-interrupted-work.md) — [03](03-feishu-flash-cards.md)/[02](02-feishu-sprint-backlog.md)/[05](05-news-and-watch.md) 常驻职责自愈的**孪生**:恢复的不是"让监听活着",而是**做到一半的一次性交付**(欠老板的那几张卡)。半成品交付 = 一条**临时承诺**,接活当下记进 commitments.md、交付即划掉;重启后读记忆-醒来-注意这个**既有回路**注意到没划掉的 loop → 先看已落什么(不重做副作用)→ 面向用户出声浮现 / 内部悄悄补完。reflection 兜 jot-before-crash 窗口(未交付承诺进 gist→hot.md)。SHIPPED 2026-06-25(57a757c),built+green,**未实测**。

> **通用视觉感官(26–27)**:把"看懂"作为一路感官接进来——不是建 CV 项目([10](10-vision-sam-yolo.md))、不是认人([16](16-recognize-people.md))、也不是收文件([18](18-send-files-to-agent.md)/[19](19-upload-passport.md)),而是 agent 自己看懂一帧 / 一段并化进记忆。两个端点:**frame**(image+prompt→文字)管"一刻",**video**(video+prompt→文字)管"一段"。今天只有 [08](08-operate-apps.md) 用到通用视觉(see-to-act),这两条补上 see-to-answer / see-to-remember / see-an-event。

- [26 · 看懂一帧:举起来当场问 / 存下来回头找](26-look-and-recall.md) — 通用视觉**最典型一路**(frame endpoint):举起实物 / 发来照片,既当场答到点(剂量、成分、跨设备读报错),这份"看懂"又留存让图按**内容**找回;升级现有固定字幕路径(`server/vision.rs`)。与 [16](16-recognize-people.md)(脸=内置模型软证据)、[19](19-upload-passport.md)(文件=物件不走感官)区分。
- [27 · 看我做,给我反馈(看一段过程,不是一帧)](27-watch-and-guide.md) — 通用视觉 **video endpoint**:看懂一段过程的先后 / 节奏 / 对错(发球、做菜),给针对性反馈、跨段对比进步;语气陪练式(连 [12](12-play-with-child.md))。区别于 [10](10-vision-sam-yolo.md)(建项目跑 CV 模型)。
