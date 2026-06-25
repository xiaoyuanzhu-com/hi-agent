# 看我做,给我反馈(看一段过程,不是一帧)

**Persona:** 用户当着摄像头做一个动作 / 一段操作,要 agent 看**一段过程**、指出问题、可反复练。
**Goal:** agent 看懂"过程"——动作的先后、节奏、对错——给针对性反馈,能跨多段对比进步。
**Preconditions:** 有通用视觉能力(video endpoint:video + prompt → 文字理解)。与 [10](10-vision-sam-yolo.md)(那条是建项目跑 CV 模型,不是通用理解)、[12](12-play-with-child.md)(语气适配同源)相连。

## Steps & expected UX

1. **"看我发球,哪儿不对"** → 用户做一次 → agent 看那段 clip(**动作有先后,必须用 video、不是单帧**)→ 指出具体点:"抛球太低、手腕没甩"。
2. **"再来一次"** → 看新一段,**对比上次**:有没有改进、还差哪。
3. **语气适配**(连 [12](12-play-with-child.md))→ 陪练鼓励式("这次手腕对了,就差抛球高度"),不是冷脸判官。
4. **(可选)留个手艺** → 同一类反复练,把"看这个动作该盯哪几点"沉成技能(连 [24](24-skill-improves-and-refreshes.md)),下次开口就盯对地方。

## Expected outcome

- 反馈针对"这一段动作"的**具体环节**,不是泛泛"挺好的";能跨多段说出进退。
- 用户感觉像边上有个**看得懂的陪练**,而不是一个只会描述画面的字幕机。

## Edge cases & failure modes

- **出画 / 太快 / 太暗** → "离远点、慢一点、开个灯再来",不对着半段瞎评。
- **太专业拿不准** → 诚实分层:"我看到的是…;不确定的是…",不硬充教练。
- **clip 太长** → 截到动作那一段再看(省成本 + 注意力),不整段硬塞。

## Open questions

- 反馈延迟:看完整段再说(准)vs 边看边提示(实时但糙)——先做哪个?
- 多段对比靠把上一段的文字理解留在上下文,还是要存下 clip 本身?(连 forgetting:clip 可褪、文字常驻)

_机制:通用视觉 video endpoint(video + prompt → 文字)+ 语气适配(连 12)+ 可选技能沉淀(连 24)。成熟度:capability seam 已在(`vision::understand` 的 `VisualMedia::Video`),endpoint 接入中,未实测。_

## 实测 2026-06-25 · worktree-vision-journeys（build+277 tests green）

- ✅ **看一段过程通**:模拟摄像头(分片 MP4 走 `WS /api/in/vision/stream`)→ 内存里的"进行中分钟"缓冲 → `watch` 切片 → Doubao Responses API(`/api/plan/v3`, `input_video`)→ 准确描述**动态变化**("数字 8 左竖段熄灭,字形改变"),证明看的是"一段"不是单帧。
- ✅ **Doubao 视频线确认**:`input_video`/`video_url`/`fps` 形状被 Ark 接受(直连 smoke + 产品路径双验)。
- ✅ **无摄像头优雅报错**:`watch` 在没有直播流时返回可读错误(让 agent 去请用户开摄像头),不 panic。
- ⚠️ **未实测**:真实浏览器 MediaRecorder(WebM)流(本次用 ffmpeg 分片 MP4 模拟,init-segment 路径相同);`watch` 的 `last Ns` ffmpeg 裁剪(本次走整段)。
