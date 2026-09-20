# tools/gui —— 用合成事件给 Tauri 应用做 GUI 验收

沙箱里 **AppleScript 走不通**：`tell application "System Events"` 与 Finder 一律返回
`-10004 权限违例`（缺「自动化」授权，且沙箱也拦）。但**「辅助功能」授权后 `CGEventPost` 可用**，
于是用两个极小的 C 工具直接驱动鼠标/键盘 —— 这就是本目录的全部内容。

```bash
cd tools/gui
xcrun clang -framework ApplicationServices -o wizact  wizact.c
xcrun clang -framework ApplicationServices -o winlist winlist.c
xcrun clang -framework Carbon          -o tis     tis.c
```

## 权限（缺一不可）

| 权限 | 位置 | 用途 |
|---|---|---|
| 屏幕录制 | 隐私与安全性 → 屏幕录制 | `screencapture`（否则黑屏/失败） |
| 辅助功能 | 隐私与安全性 → 辅助功能 | `CGEventPost` 注入鼠标/键盘 |

授权后**先自检**，别猜：

```bash
screencapture -x /tmp/t.png            # 屏幕录制
./wizact move 700 450                  # 辅助功能：应打印 want/got 一致
```
`wizact move` 若 `got` 与 `want` 不同 ⇒ 辅助功能没开，**注入会被静默丢弃**（不报错）。

## wizact —— 输入注入

```bash
./wizact pos                          # 打印当前光标（逻辑点）
./wizact move  700 450                # 移动并回读
./wizact click 546 483                # 单击（先 move 再 down/up）
./wizact dclick 546 483
./wizact drag  80 27  -1200 300       # 拖窗口：起步点 → 终点
./wizact type  "文本"                  # 任意 Unicode（CGEventKeyboardSetUnicodeString，中文/路径都能打）
./wizact key   53                     # 单键（53=Esc，36=Return，48=Tab，55=⌘，4=H）
./wizact chord 4 cmd                  # **物理按下 ⌘ 再敲 H**（cmd|shift|cmdshift|ctrl）
./wizact appswitch 2                  # ⌘Tab 循环 n 次（同样带物理 ⌘）
```

`chord` / `appswitch` 与 `cmdkey` 的区别很关键：**只给事件打 ⌘ 标志、不真按 ⌘ 键，
菜单快捷键（⌘H/⌘M）和 ⌘Tab 切换器会不生效或卡住切换器**。

## winlist —— 窗口枚举（比截图更可信）

```bash
./winlist | grep "layer=0"            # 每行：layer / pid / owner / x y w h（逻辑点）
```

用途：**判断「有没有弹窗」不要靠截图**——合成操作期间合成器可能给你旧帧；
数窗口才是硬证据（本次验收就靠它证明「导入全程只有 1 个窗口」）。
Tauri 的原生文件面板会以**同一 pid 的第二个窗口**出现，`winlist` 一眼可见。

## 坐标换算（三层，容易错）

| 层 | 关系 |
|---|---|
| `CGEvent`/`winlist` 坐标 | 逻辑点（主屏左上为 (0,0)） |
| `screencapture -x` 出的 PNG | 逻辑点 ×**2**（Retina 2880×1800 ⇒ 1440×900） |
| 把 PNG 丢给模型看时的预览 | 再 ×0.75（1080 宽） ⇒ 预览 px × **1.3333** = 逻辑点 |

即：**在预览图上量到的坐标 × 1.3333 = 直接喂给 `wizact click` 的值**。
多显示器时副屏在**负坐标**区，`screencapture -x` 默认只抓主屏。

## tis —— 输入源切换（打 ASCII 前必用）

```bash
./tis list                  # 列出全部输入源（* = 当前选中）
./tis select keylayout.ABC  # 切到 ABC 键位
./tis select aodaren        # 测完切回「青鸽」拼音
```

**为什么需要**：系统在拼音输入法下，`wizact type` 的事件会被 IME 拦截进
候选窗（合成缓冲），落进文本框的是候选字而不是你打的串，**且无任何报错**。
打 ASCII（检索词、新标题）前先切 ABC，测完务必切回来（这是用户的机器）。
剪贴板路线（pbcopy + ⌘V）在本执行环境**不可靠**（写入后读回常为 0 字节），别依赖。

## 踩过的坑

- **别用 ⌘H 去「隐藏挡路的应用」**：焦点会变，很可能把**被测应用自己**藏了
  （症状：进程还在、`winlist` 里没有它的窗口、没有报错）。恢复用 `open -a <app>`（会 unhide）。
- 想让被测应用到前台：`open -a <app>`；确认前台看**菜单栏标题**（截图左上角）。
- `screencapture -C` 带光标，用于**校准**注入坐标是否真的落到目标上。
- 输入框/路径优先用 `type`（Unicode 直发），别逐个 `key`。
- 被测应用若在执行**同步 Tauri 命令**（如 `build_index_cmd`）会占住主线程 ⇒
  期间窗口不重绘、提示文案停在上一状态。**这不是截图坏了**，用 `winlist` + 磁盘变化交叉判断。
- **应用非前台时合成点击会被「激活窗口」吃掉**：表现为点了没反应（悬停高亮有、动作无）。
  先 `open -a <app>` 再点。用户同时在用机器时，光标会被物理鼠标不断挪走，
  `wizact click` 回读的 `got` 与 `want` 不一致即是信号 ⇒ 关键点击点前后各查一次落点。
- **`screencapture -l <winid>` 抓单窗口带阴影内边距**（1704px 图对应 740pt 窗），
  不能直接拿它量点击坐标；量坐标一律用 `-R x,y,w,h` 区域截图。
  但 `-l` 是**窗口自身缓冲**，可用来判「内容真空白」还是「截图旧帧」。
- 对小按钮的目测坐标常有 ±20px 误差：点多次没反应时，**在目标邻域做网格点击**
  （步长 10–12px，点完立刻查落盘结果），比反复目测快得多。
