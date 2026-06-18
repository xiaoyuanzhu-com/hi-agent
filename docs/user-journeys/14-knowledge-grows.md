# 你对 YOLO 的了解,随用而长、也被实践修正

**Persona:** 同一个用户在不同时间问 agent 关于 YOLO 的事;agent 也在这期间真的用过 YOLO。
**Goal:** agent 对 YOLO 的"懂"应随**用过的次数 / 新近度**自然变深;一次真实失败 + 找到更好的 SAM,应**改写**出厂先验。
**Preconditions:** 有记忆(facet + episode 证据图)与可被实践盖过的出厂先验(`prompts/world.md`)。**底层模型见 [data-dir-layout](../data-dir-layout.md) 的 E1/E3;与 [10](10-vision-sam-yolo.md) 相连。**

## Steps & expected UX

1. **早期问"你会 YOLO 吗"** → 浅:可能只凭出厂先验 / 读过的文档作答(深度浅,因为还没"做过")。
2. **agent 实际用过几次后再问"YOLO 怎么调 / 坑在哪"** → 更深:能说出**实战坑**(如对小目标 / 旋转文本不行),因为有"做过"的 episode 撑着。**深浅是读出来的,不是存了个"等级"。**
3. **一次失败 + 试 SAM 成功** → agent 写一条带"我试过"出处的实践认识,**盖过**出厂"YOLO 适合 X"的先验。
4. **此后再问"圈这个用什么"** → 答 **SAM**(给出"我试过、YOLO 在这类上不行"的理由),不再背出厂先验。

## Expected outcome

- 同一问题,**用得越多答得越深**(实战细节出现),且能追溯到"做过"的经历。
- 出厂先验被一次真实经验**安静地超越**;再问以实践为准。

## Edge cases & failure modes

- 只读过文档没做过 → 应诚实区分"我读到…"与"我试过…",前者口气更弱。
- 出厂先验与新实践冲突 → **实践(有证据)胜**;不死守出厂默认。

## Open questions

- "懂多少"完全在读时由证据图算出,还是 reflection 也写一个粗粒度 confidence?(data-dir-layout fork)
- 出厂先验(`world.md`)放哪、怎么更新、被实践 claim 超越时的优先级如何判定?

_机制:competence = 证据图(读出,不存等级)+ provenance(authored < read < did)+ 先验被 lived 超越。成熟度:**依赖知识模型整套(未建)**——这是验证 provenance / world.md 的核心 journey。_
