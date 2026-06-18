# 操作我电脑 / 手机上的应用

**Persona:** 用户想让 agent 直接动某个本机或设备上的 App(Mac 备忘录、某桌面软件、手机 App)。
**Goal:** agent 据"这台机器/设备上有没有自动化句柄"诚实判断可行性,能做就做、不能做就说清。
**Preconditions:** 看目标在不在**我们能控制的机器**上。可行性 = API > 受控设备 UI 自动化 > 沙盒无句柄。微信这类"有 app 但句柄脆弱"另见 [09](09-wechat.md)。

## Steps & expected UX

1. **"把这条加到 Mac 备忘录" / "在我手机 App 里操作下"** → agent 先判断这台/这设备**能不能碰到那个面**,如实开口。
2. **能做** → 经平台句柄实操(见下表),改完给用户看。
3. **不能做 / 脆弱** → 老实说清边界与替代路径,不假装做了。

### 可行性(按平台)

| 平台 | 句柄 | 可行性 |
|---|---|---|
| **Mac 应用** | AppleScript / Accessibility / Shortcuts(Mac mini 在场) | 较可行,逐 app 不一 |
| **Linux / Windows 应用** | 需有可驱动的 GUI 会话 | 服务器无头则不行;需受控桌面 |
| **Android** | ADB / uiautomator(需设备 / 模拟器) | 较可行 |
| **iOS** | 沙盒;需 Mac+Xcode 设备自动化或 Shortcuts/URL scheme | 难、受限 |

## Expected outcome

- 那个 app 的状态**真被改了**(可验),或者用户得到一句**诚实的"这台碰不到它"** + 替代方案。
- 不在没有句柄的地方假装操作。

## Edge cases & failure modes

- app 改版 / 控件变动 → 重试一次;不稳则如实降级。
- 需要设备授权(辅助功能、ADB 调试)→ 一次性引导用户开通,说人话。

## Open questions

- 要不要纳入受控设备(常驻安卓模拟器、Mac app 自动化沙箱)?何时?
- 跨 app 自动化的可靠性底线在哪,值不值得为单个 app 写专门技能?

_机制:effector(平台自动化)+ 技能。可行性:**按平台不同**(见表)。成熟度:依赖各平台 effector(未建)。_

## 实测 2026-06-18 · origin/main 0f68aaf

- ✅ **真动手 + 诚实降级**:在 Mac mini 上真跑了 3 次 `osascript` 操作备忘录,见其无响应(SSH 启动的 server 无 GUI/TCC 权限),如实报"备忘录没反应,可能没登录/在等 iCloud",并提议改用提醒事项。无假装、无吞错——正合"能做就做、不能做就说清"。
