# 用 SAM / YOLO 做点视觉活

**Persona:** 用户给一张图 / 一段视频,要 agent 做检测 / 分割(把人圈出来、数一下、抠个掩膜)。
**Goal:** agent 用合适的视觉能力真跑出结果;首选不灵就换方案(并把"哪个适合什么"沉淀)。
**Preconditions:** 有视觉能力(本地 ONNX 或厂商 API);技能记着"怎么跑"。与 [14](14-knowledge-grows.md) 相连——"YOLO 试了不行换 SAM"正是先验被实践修正的例子。

## Steps & expected UX

1. **"把这张图里的人圈出来 / 数数有几只"** → 接住,选一个合适的模型 / 能力实跑。
2. **跑出结果** → 把掩膜 / 检测框作为产物呈现(落 `views/` 或直接交付);简短说结论。
3. **首选不灵**(YOLO 漏检小目标)→ **主动换方案**(换 SAM / 换参数),把"YOLO 适合 X、SAM 适合 Y"写成带"做过"出处的认识(见 [14](14-knowledge-grows.md))。

## Expected outcome

- 真有掩膜 / 检测结果产出且可见,不是"你可以用 YOLO 试试"的纸上谈兵。
- 失败转方案是**主动**的;经验沉淀,下次少走弯路。

## Edge cases & failure modes

- 没配视觉能力 → 置备(本地 ONNX 自带二进制 / 厂商 API,见 [13](13-equip-a-capability.md));缺就先请示装。
- 图太大 / 格式怪 → 预处理;跑不动如实说,不假装。

## Open questions

- 产物归宿:一次性结果落 `views/`(用完即弃)还是值得留的进 `drive/projects/`?
- 多个候选模型时怎么选——固定偏好还是按图判断?

_机制:技能(怎么跑)+ 能力(模型 / API)+ 先验修正(连 14)。可行性:**可行**。成熟度:依赖技能层 + 通用检测/分割能力 wired(face/voiceprint 已 staged,通用视觉未 wired)。_
