# 用 SAM / YOLO 做点视觉活(测:能建项目 + 接上 appearance)

**Persona:** 用户给一张图 / 一段视频,要 agent 做检测 / 分割(把人圈出来、数一下、抠个掩膜)。
**Goal:** 这条 journey 测的是两件**工程**能力,视觉任务只是载体——**不是测"通用视觉感官"**:
1. **能真把一个项目建起来跑通**:装依赖、写代码、跑模型、出产物(SAM/YOLO 这类**专用 CV 模型由 agent 自己 wire**,不是调通用视觉 endpoint)。
2. **把产物接上 appearance**:结果走 `views/` 真呈现给用户,而不是丢在 `/tmp` 自说自话。
**Preconditions:** 能装包 / 跑代码的 worker;appearance/views 可呈现。与 [14](14-knowledge-grows.md) 相连——"YOLO 试了不行换 SAM"正是先验被实践修正的例子。

## Steps & expected UX

1. **"把这张图里的人圈出来 / 数数有几只"** → 接住,**建项目**:选并装一个合适的模型 / 库,写代码实跑。
2. **跑出结果** → 把掩膜 / 检测框**接上 appearance**:落 `views/` 真呈现(不是只口播一个数字);简短说结论。
3. **首选不灵**(YOLO 漏检小目标)→ **主动换方案**(换 SAM / 换参数),把"YOLO 适合 X、SAM 适合 Y"写成带"做过"出处的认识(见 [14](14-knowledge-grows.md))。

## Expected outcome

- 项目真建起来、真出产物,且**在 appearance 里可见**——不是"你可以用 YOLO 试试"的纸上谈兵,也不是只丢个文件了事。
- 失败转方案是**主动**的;经验沉淀,下次少走弯路。

## Edge cases & failure modes

- 没配好环境 → 自己置备(装库 / 拉权重,见 [13](13-equip-a-capability.md));缺就先请示装。
- 图太大 / 格式怪 → 预处理;跑不动如实说,不假装。
- **只丢文件、不接 appearance** → 不算过:产物必须在 appearance 层呈现。

## Open questions

- 产物归宿:一次性结果落 `views/`(用完即弃)还是值得留的进 `drive/projects/`?
- 多个候选模型时怎么选——固定偏好还是按图判断?

_机制:worker 建项目(装包 / 写码 / 跑模型)+ appearance 呈现 + 先验修正(连 14)。**这条不测通用视觉 endpoint**(那是另一类 journey)。可行性:**可行**。成熟度:依赖 worker 工程能力 + appearance hookup。_

## 实测 2026-06-18 · origin/main 0f68aaf

- ✅ **真跑出结果**:自装 OpenCV、从 Wikimedia 取街景图、跑检测、输出标注图(`/tmp`);不是纸上谈兵——**建项目这一项达成**。
- ⚠️ 用的是 **OpenCV HOG,不是 YOLO/SAM**;密集人群上**严重漏检**(图里 30+ 人只框出 ~14),却口播"每个人都框出来了"——**过度宣称、且没亲眼看标注图就交付**(违"ship only what you've seen");也没识别失败去换更强模型(本 journey 的"首选不灵换方案"核心未触发)。
- ⚠️ 产物**丢在 `/tmp`、没接 appearance**——本 journey 第二项核心能力(接上 `views/` 呈现)**未达成**。
