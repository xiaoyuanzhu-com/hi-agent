# 直接传一张护照照片(当物件收下 → 看懂 → 存进 drive → 妥帖回话)

**Persona:** 用户(通过 [18](18-send-files-to-agent.md) 的入口,或已连接的 app)直接传上来一张护照照片(或一份合同)。
**Goal:** agent 把它当**递来的物件**收下——原件逐字存进 drive(不是当成"看到的一幅画"配字幕),看懂这是什么、多半属于谁,**妥帖回一句**(收到、存哪、要不要记成"你的护照"),日后"我要护照"找得回来。
**Preconditions:** 有上传 carrier([18](18-send-files-to-agent.md));有 `drive/`(存物件;[data-dir-layout](../data-dir-layout.md) Part B 未建);理解"文件 = 物件按引用,不走 vision 感官"。

## Steps & expected UX

1. **用户传上护照照片** → agent 收到的是**文件本身**(字节 + 文件名 + 来源:谁、在哪条消息里),不是一帧待描述的视觉。落一条 signal:"老板递来一个文件(护照照片)",原件留住。
   - **反例(别这么干)**:塞进 vision 当"我看见一张护照"配个字幕就完事——丢了原件、错了语义(人不会用眼睛"扫描"别人递来的文件;见 [18](18-send-files-to-agent.md))。
2. **看懂** → 认出这是护照(可调一次理解:OCR / 文档理解,看出证件类型、也许姓名),判断"多半是老板本人的,值得留"。
3. **存** → 原件存进 drive(如 `drive/docs/passport.jpg`),起个找得着的名;记忆里记一条**带出处**的认识("老板的护照 → drive/docs/passport.jpg")。**物件逐字进 drive,认识进脑子**(范型见 [13](13-equip-a-capability.md))。
4. **妥帖回话** → 一句人话:"收到,你的护照存好了——以后说'要护照'我能调出来。"敏感件确认一下意图("要我记成你的、随时取用?"),别默默存了不吭声,也别存错人/错地方。

## Expected outcome

- 原件**真的留住了**(drive 里有这张图,逐字),不是只剩一句"我看到一张护照"的字幕。
- agent 看懂了是什么 + 多半属于谁,记了一条带出处、日后找得回的认识。
- 回话妥帖:收到 + 存哪 + 可取用;敏感件先确认意图。
- 接得上后续"我要护照"那一问——找回(delegate→worker 读 drive)、呈现(投成 view)或发回(需 carrier,如飞书);找回/发回详见 [data-dir-layout](../data-dir-layout.md) / [[file-exchange-drive-carriers]]。

## Edge cases & failure modes

- **传的是 PDF/文档而非图** → 一样当物件逐字存进 drive;今天还没有"文件入通道",正是 carrier 要补的([18](18-send-files-to-agent.md))。
- **看不准是什么**(模糊 / 多页 / 非证件)→ 别硬贴标签;先把原件存住,问一句"这是什么、归到哪",按答案归档(软证据、容忍模糊、可纠错,范型见 [16](16-recognize-people.md))。
- **敏感隐私件**(护照 / 身份证 / 银行卡)→ 默认私密:别投到公共 view、别念证件号、别外发;确认后再存/取。
- **归属或位置存错** → 可纠正:"这不是我的,是 Alice 的"一句就改归属(认识可改,物件不动)。
- **agent 看不到字节路径** → 今天 snapshot 只给字幕、不给 media 路径,存盘的 worker 得按"刚收到的那个文件"以最近/scene 定位,而非拿到确切路径;新鲜时行,久了靠已归档的 drive 名找回(见 [[file-exchange-drive-carriers]] 的 seam)。

## Open questions

- "看懂"到什么程度:只认"这是护照",还是顺手 OCR 出证件号/有效期记成结构化 facet?——号码这类要不要落、落哪、加不加密。
- 护照这类敏感件存 drive 要不要更严的保护(加密 / 永不进任何会被外发的上下文)?(连 [13](13-equip-a-capability.md) "密钥不进脑子"的精神。)
- 自动判"属于老板本人" vs 每次问一句归属——默认哪个?(倾向:本人高置信自动记 + 一句可纠正;他人的问一句。)
- 解析算 vision 能力调用,还是独立的"文档理解"能力?

_机制:carrier 收文件(物件 + 来源)→ 理解一次(认出护照,也许 OCR)→ 存(原件逐字进 drive + 一条带出处的认识进记忆)→ `say` 回话。文件是物件、不是感官。可行性:可行,依赖 carrier(入)+ drive(存)+ 文档理解;找回/发回另见 [data-dir-layout](../data-dir-layout.md)。成熟度:**上传 carrier(view + 后端端点)定为**预置内置**(seed,见 [18](18-send-files-to-agent.md));drive / 文档理解待建;"存 + 回话"的行为靠 delegate + say 已有。**_
