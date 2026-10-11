<h1 align="center">
  <a href="https://github.com/llight24/Snaplight"><img src="icon/snaplight-logo-horizontal.svg" alt="Snaplight" width="380" /></a>
</h1>

<p align="center">
  <a href="https://github.com/llight24/Snaplight"><img src="https://img.shields.io/github/stars/llight24/Snaplight?style=flat&amp;label=%E2%98%85&amp;color=08C" alt="GitHub stars" /></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-08C?style=flat" alt="License: MIT" /></a>
  <img src="https://img.shields.io/badge/platform-Windows%2010%20%7C%2011-4493F8?style=flat-square" alt="Platform: Windows 10 / 11" />
  <img src="https://img.shields.io/badge/Rust-2021-000000?style=flat-square&amp;logo=rust&amp;logoColor=white" alt="Rust 2021" />
  <img src="https://img.shields.io/badge/UI-Slint-4A86CF?style=flat-square" alt="UI: Slint" />
</p>

<p align="center">
  <sub><a href="README.md">中文</a> · <a href="docs/readme/README.en.md">English</a></sub>
</p>

<p align="center">
  <strong>Windows 优先的截图、标注与贴图工具。</strong><br/>
  原生 Direct2D / DirectComposition 画布，Slint 负责工具栏与设置；常驻托盘，空闲时阻塞等待、不做周期轮询。
</p>

---

## 功能

### 截图捕获

- **按适配器捕获桌面**：使用 Desktop Duplication 抓取每个显示器，并把结果复制到 Snaplight 自己的纹理；会把首帧“无桌面更新”的情况跳过，最多等待 1.5 秒。
- **明确的降级路径**：Desktop Duplication 不可用或抓取失败时回退到 GDI 抓屏；D3D11 硬件设备创建失败时使用 WARP 软件光栅化。
- **冻结底图与不可变原件**：覆盖层显示前先冻结底图，标注只记录在文档模型上，原始像素不会被改写，因此撤销/重做始终可用，导出也不会累积画质损失。
- **HDR → SDR**：高色深输入经过色彩空间转换与色调映射后输出 sRGB，并在图像元数据中标记为 HDR→SDR。
- **有界失败处理**：捕获失败重试固定次数（3 次、每次间隔 300ms）后明确报错退出，不做无限重试；设备丢失或 `D2DERR_RECREATE_TARGET` 时用保留的底图重建纹理、交换链与合成目标并继续编辑，`WM_DISPLAYCHANGE` 同样走重建而不是结束会话。

### 选区与裁剪范围

- 拖动画出选区，选区外按 55% 变暗，便于确认截取范围。
- 选区外观：蓝色描边（`#3388ff`，2.2 逻辑像素）、8 个圆形手柄（白色外圈 + 蓝色内芯，外径 13 / 内径 9.2 逻辑像素）、深色圆角信息条显示屏幕坐标与像素尺寸（`x,y  W × H px`），所有尺寸按显示器 DPI 缩放。
- **八个锚点调整裁剪范围**：四角同时改变宽高，四边中点只改变一个轴，对边固定；限定在图像边界内、最小 1×1 像素，拖过对边时停在最小尺寸而不翻转；一次完整拖动只产生一条可撤销历史。悬停时显示对应的缩放光标。
- **放大镜**：显示光标附近的 20×20 源像素区域，6× 放大，白色边框与十字准星，贴近图像边缘时自动夹取。
- 重新框选期间工具条与快捷设置自动隐藏，完成后重新定位：优先放在选区下方，放不下则放上方，并夹在捕获范围内。撤销掉最后一个选区会隐藏工具条，重做恢复。
- 目前选区限制在单个显示器内。

### 标注工具

| 工具 | 说明 |
| --- | --- |
| 选择 | 框选并拖动八个锚点调整裁剪范围 |
| 矩形 | 拖拽绘制矩形，线宽与颜色可调 |
| 椭圆 | 拖拽绘制椭圆，线宽与颜色可调 |
| 箭头 | 从起点到终点绘制箭头 |
| 画笔 | 自由笔画，按轨迹逐段绘制 |
| 马赛克 | 按块大小打码，默认 12 像素块 |
| 文字 | 在快捷设置里输入文字，然后点击画布放置 |

- **每个绘图工具有独立的快捷设置**：点击工具右侧的下拉箭头打开，可调线宽、颜色（红/绿/黄/白）、马赛克块大小与文字内容；面板按工具条上方或下方定位，并夹在捕获范围内。
- **白色胶囊工具条**：图标全部用 Slint `Path` 矢量绘制，统一 24×24 网格、1.75px 线宽、圆角端点的线性风格；含拖动手柄、分组分隔线、工具下拉箭头；撤销/重做在没有可撤销历史时灰显。
- **宽度自适应**：工具条宽度按 `Config.tools` 里的绘图工具与操作按钮列表计算，命令 ID 与显示顺序解耦，增删或调换按钮不需要改编辑器命令映射；拖动期间隐藏，只由交互事件刷新，没有常驻帧循环。
- 标注绘制在 GPU（Direct2D）上并缓存为静态图层，只有变化的部分重绘；马赛克与放大镜同样在 GPU 上完成。

### 编辑、导出与剪贴板

| 快捷键 | 作用 |
| --- | --- |
| `Ctrl+Z` / `Ctrl+Y` | 撤销 / 重做（包括选区调整） |
| `Ctrl+C` | 把选区复制到剪贴板（DIB 格式） |
| `Ctrl+S` | 保存 PNG |
| `Enter` | 把选区钉为贴图 |
| `Esc` / 鼠标右键 | 取消本次截图 |

- **保存内容完整**：每次保存写出 `<时间戳>.source.bgra`（原始 BGRA 原件）、`<时间戳>.document.json`（版本化文档，含选区与全部标注）以及 `<时间戳>.png` 或贴图图像，因此保存的截图可以再次载入编辑，而不只是一张成品图。
- 导出使用 GPU 回读；需要时也可以走 GDI 降级路径。

### 贴图

- **单进程统一管理**：`snaplight-pins.exe` 管理所有贴图窗口；关闭最后一张贴图后该进程自动退出，不留下常驻窗口进程。
- **位置与尺寸不变**：新贴图在它被截取的那个屏幕位置打开（由编辑器随命令下发截取区域的屏幕物理坐标），只有宽于主屏时才缩放，钉住前后位置与尺寸一致。
- **交互**：拖动移动；滚轮缩放（每格 1.1×）；`Ctrl+滚轮` 调节透明度（每格 5%，范围 10%–100%）；双击恢复 100%。
- **右键菜单**：鼠标穿透（`Ctrl+Shift+P` 恢复）/ 原始大小 / 缩小一半 / 关闭。穿透模式保留可恢复交互的全局入口，不会让贴图失去操作方式。
- **不抢焦点**：贴图窗口用 `MA_NOACTIVATE` 响应鼠标激活，点击贴图不会把焦点从当前程序抢走。
- **两种后端**：默认静态贴图使用 layered window（低私有内存）；在设置里开启「GPU 贴图」后改用 DirectComposition 持久 surface。GPU 贴图进入穿透模式时会把缩放重置为 100%。
- 隐藏贴图时释放纹理，但尚未按合成器遮挡回收（见[已知限制](#已知限制与后续计划)）。

### 光标下界面元素识别

- 在设置里把窗口识别设为「光标下的界面元素」后，未选中任何区域时**自动高亮光标下的 UI Automation 元素**：仅有蓝色描边，没有手柄和尺寸信息条，因此不会被误认为已完成的选区；左键单击直接采用该区域。
- 仍然可以拖动手动框选；开启识别时，点击没有命中任何元素则不会提交选区，关闭识别时保持原有行为。
- 元素链策略：从光标元素向上最多 10 级找到 `ControlType == Window` 的顶层窗口，再在元素与窗口之间取第一个两轴都不小于 8 物理像素的矩形，都没有时回退窗口矩形。
- 桌面、任务栏以及没有暴露 UI Automation 提供者的窗口不会产生高亮。
- 实现方式是在光标处给覆盖层窗口区域临时打一个 1×1 的洞，让 UI Automation 命中下层元素，查询后立即恢复；查询在覆盖层线程内联执行（实测 5–33ms），拖动时按 ≥3 物理像素且 ≥33ms 节流，排队中的探测点会更新为最新位置。
- 设置项在每次截图会话开始时读取，改动对下一次截图生效；识别模式是枚举，后续可以追加像素识别而不改变配置结构。

### 常驻核心与全局快捷键

- `snaplight-core.exe` 常驻托盘，只运行阻塞式 Win32 消息循环：**不初始化 Slint、GPU、OCR 模型或录屏线程**。
- 托盘菜单：截图 / 设置 / 关闭所有贴图 / 退出。
- 全局快捷键可配置，写成 `Ctrl+Shift+A` 形式（大小写不敏感、修饰键顺序任意、至少一个修饰键，支持 A–Z、0–9、F1–F24）：默认 `Ctrl+Shift+A` 开始截图，`Ctrl+Shift+P` 恢复所有穿透贴图的交互。快捷键被其它程序占用时回退默认并打印原因；无法解析的快捷键在加载配置时就被拒绝。
- 子进程按需启动与回收：编辑器随截图会话退出，贴图管理器在最后一张贴图关闭后退出，插件宿主一次任务一个进程；清理使用持有的进程句柄，避免 PID 重用误杀其它进程。

### 设置界面（中文优先）

设置窗口是「左侧功能导航 + 右侧设置选项」，共 7 页：

| 页面 | 内容 |
| --- | --- |
| 常规 | GPU 贴图开关；绘图工具排序（逐项上移/下移，稳定工具 id 与显示顺序解耦） |
| 外观 | 主题背景色、前景色、强调色 |
| 快捷键 | 截图快捷键与恢复贴图快捷键 |
| 截图 | 绘制默认值（线宽、马赛克块）、窗口识别模式、保存目录（可直接输入或用系统文件夹选择器）、已保存截图列表（本地时间 / 格式 / 占用大小，支持打开、打开文件夹、删除到回收站；删除需要二次确认） |
| 翻译 | 占位页，明确说明尚未实现 |
| 屏幕录制 | 占位页，明确说明尚未实现 |
| 设置 | 应用版本、配置文件路径、数据目录与「打开目录」按钮 |

- 工具栏快捷选项、托盘菜单、贴图右键菜单与设置页文案全部**中文优先**；代码标识符、命令和错误码保留原文。
- 可访问性：Slint 通过 `accessibility` 特性暴露无障碍树（标签、按钮、输入框、复选框等），Windows UI Automation 与屏幕阅读器可以枚举并驱动设置页与工具条，控件名称就是用户看到的中文标签。
- 工具条与设置窗口的位置记录在 `ui-state.json`；文件缺失、损坏或版本不匹配时只回退到默认位置。
- 状态提示会明确说明结果，例如保存成功、需要再次点击「确认删除」、已把 N 个文件移入回收站、快捷键格式无效且未写入配置。

### 配置与数据目录

- 配置与截图默认保存在 `%LOCALAPPDATA%\Snaplight`，可用环境变量 `SNAPLIGHT_DATA_DIR` 覆盖。
- `config.json` 保存主题颜色、线宽、马赛克块大小、工具顺序、窗口识别模式、GPU 贴图开关、保存目录与两个全局快捷键；`ui-state.json` 保存窗口位置。
- 截图默认保存在 `<数据目录>\captures`（可在设置中更改）。
- 设置里的「翻译」和「屏幕录制」页只是占位：**功能没有实现，就不会有设置项假装它能用**。

### 插件与扩展

- 插件使用版本化 C ABI（`extern "C"` + `#[repr(C)]`），不跨 ABI 传递 Rust 集合、trait 对象或原生 GPU 对象。
- 独立宿主进程隔离故障：插件崩溃不会带走常驻核心；宿主进程只用于故障隔离，不等同于不可信插件的安全沙箱。
- 大图通过只读共享内存传递，控制消息使用有界、版本化的 JSON 协议；也支持命名 GPU 共享纹理（`SHARED_NTHANDLE | SHARED_KEYEDMUTEX` + keyed mutex 握手、有界超时），已在真实进程边界验证。
- 宿主在打开映射和加载动态库之前校验传输元数据与 ABI 版本/结构尺寸，插件未声明能力位时拒绝相应请求；取消、超时、强制退出和动态库卸载顺序都有明确契约。详见 [插件 ABI 契约](docs/plugin-abi.md)。
- `plugins/example` 是**诊断用的 inspect 插件**（读取图像并计算校验和），不是 OCR 引擎，也不能当作 OCR 功能验收。

### 性能与资源

测量环境：Windows，Intel Core i9-14900KF（32 逻辑 CPU），NVIDIA RTX 5060 Ti，3840×2160 / 60Hz，Release 构建，CPU 采样 5 秒。

| 场景 | 私有工作集合计 | 私有提交合计 | 共享提交合计 | CPU 增量 |
| --- | ---: | ---: | ---: | ---: |
| 无贴图托盘待机 | 1.07 MiB | 1.51 MiB | 9.02 MiB | 0 ms |
| 五张 1920×1080 贴图 | 2.25 MiB | 3.29 MiB | 57.61 MiB | 0 ms |
| 两张 3840×2160 贴图 | 2.25 MiB | 3.27 MiB | 49.68 MiB | 0 ms |
| 二十张 1920×1080 贴图 | 2.21 MiB | 3.42 MiB | 168.40 MiB | 0 ms |

- 统计口径是**所有自有常驻进程的私有工作集与私有提交之和**，共享提交单独列出。如果把共享图像缓冲也算进系统内存，持续贴图的总占用并不低于 30 MB；layered window 不会消除图片内存和 DWM 的 GPU 资源，贴图数量与分辨率也不设产品级硬上限。
- 4K 底图 + 1000 个标注 + 每帧重绘放大镜与 600 点实时笔画的压力模式（Release，241 帧）：CPU 提交 p50 0.902–0.960ms、p95 1.48–1.58ms、p99 1.83–1.87ms、max 3.40–3.49ms，全部低于 120Hz 的 8.33ms 预算。
- CPU 提交耗时、Present 等待和 D3D11 GPU 时间戳是三个不同指标，分别记录、不互相替代。完整测量方法与限制见 [性能报告](docs/performance.md)。

---

## 快速开始

```powershell
.\scripts\launch.ps1            # Release 构建 + 启动托盘进程 + 打印手动测试清单
.\scripts\launch.ps1 -SkipBuild # 不重新构建，直接启动
.\scripts\launch.ps1 -Settings  # 直接打开设置窗口
.\scripts\launch.ps1 -Restart   # 停止 → 重新构建 → 再启动
.\scripts\launch.ps1 -Stop      # 优雅停止全部 Snaplight 进程
```

基本流程：

1. 点击托盘图标，或按 `Ctrl+Shift+A`。
2. 拖动框选区域；也可以直接单击由识别高亮的界面元素。拖动手柄调整裁剪范围。
3. 选择工具并绘制；点工具右侧下拉箭头调整线宽、颜色、马赛克块或文字。文字输入后在画布上点击放置。
4. `Ctrl+Z` / `Ctrl+Y` 撤销重做，`Ctrl+C` 复制到剪贴板，`Ctrl+S` 保存 PNG，`Enter` 钉住选区，`Esc` 取消。
5. 拖动贴图移动，滚轮缩放，`Ctrl+滚轮` 调整透明度，右键打开贴图菜单；`Ctrl+Shift+P` 一次性恢复所有穿透贴图的交互。

这是编译型原生程序：**没有热重载**，Windows 也不允许替换正在运行的可执行文件，所以改代码后必须 `-Stop` / `-Restart` 再重新构建。

---

## 安装与构建

要求 Windows 10/11 与 Rust 工具链。使用标准 Rust MSVC 工具链 + Windows SDK 时可以直接用 Cargo：

```powershell
cargo build --workspace --locked
cargo test --workspace --locked
.\target\debug\snaplight-core.exe
```

本仓库也支持通过 `scripts/dev.ps1` 使用放在忽略目录 `.tools` 里的本地 GNU/LLVM 工具链：

```powershell
.\scripts\dev.ps1 build -Release
.\scripts\dev.ps1 test
.\scripts\dev.ps1 clippy
.\scripts\dev.ps1 fmt
.\scripts\dev.ps1 smoke -Release
.\scripts\dev.ps1 run -Release
```

发布时需要一起分发四个可执行文件：`snaplight-core.exe`、`snaplight-editor.exe`、`snaplight-pins.exe`、`snaplight-plugin-host.exe`（以及 `plugins/` 下的插件动态库）。工具链目录是机器本地目录，不进入版本库。

应用与窗口图标统一来自 `icon/`：`icon.ico` 由 `icon.svg` 生成并包含 Windows 需要的尺寸（16/24/32/48/64/128/256），每个可执行文件通过 `scripts/build/app_icon.rs` 以资源 id 1 嵌入，托盘图标与窗口 `WM_SETICON` 复用同一份资源；替换图标意味着重新生成 `icon.ico` 并重新构建。

---

## 开发与验证

| 命令 | 作用 |
| --- | --- |
| `.\scripts\dev.ps1 build -Release` | Release 构建 |
| `.\scripts\dev.ps1 test` | 先构建 workspace 再运行全部测试（插件宿主集成测试会从构建目录加载示例插件动态库） |
| `.\scripts\dev.ps1 clippy` | 全目标 Clippy（`-D warnings`） |
| `.\scripts\dev.ps1 fmt` | 格式检查 |
| `.\scripts\dev.ps1 smoke -Release` | 端到端 smoke：真实捕获、确定性像素检查、界面截图、贴图后端、示例插件 |
| `.\scripts\resource-scenarios.ps1 -Profile release -Seconds 10` | 空闲 / 五张 1080p / 两张 4K / 二十张贴图的资源场景 |
| `.\scripts\measure-resources.ps1` | 汇总所选构建目录中各可执行文件的私有工作集、私有提交、共享提交、CPU 增量与可用 GPU 计数器 |

smoke 会对真实捕获做像素级断言（并与同一屏幕的独立 GDI 抓屏按稀疏采样比较，防止“全黑底图”静默通过），为设置页每一页和三种快捷选项各保存一张截图，校验工具条物理尺寸与 DPI 换算、放置位置落在捕获范围内、窗口位置经 `ui-state.json` 往返回归，以及贴图打开位置与缩放前后一致。`snaplight-editor.exe --stress` 会在真实 4K 捕获底图上渲染 240 帧压力负载并报告 CPU 提交、Present 与 GPU 时间戳分位数。

它**不**模拟真实的跨进程鼠标输入，也不证明刷新率性能。按改动风险选择验证方式：领域与协议改动跑相关测试，窗口/图形/生命周期改动跑 Release smoke，性能改动重新测量对应负载；纯文档改动只需核对内容与实现、命令、路径是否一致。

---

## 架构

进程边界：

```text
snaplight-core.exe            常驻托盘：阻塞 Win32 消息循环、托盘菜单、全局快捷键、子进程调度
 ├─ snaplight-editor.exe      按需截图：捕获、标注画布、工具条、设置窗口
 ├─ snaplight-pins.exe        统一管理所有贴图，关闭最后一张后退出
 └─ snaplight-plugin-host.exe 一次任务一个宿主进程，插件崩溃不影响常驻核心
```

模块划分：

| Crate | 职责 |
| --- | --- |
| `crates/domain` | 坐标、图像契约、文档、标注、历史、有界 LRU 缓存、事件需求合并、配置与版本化 IPC；不依赖 Win32、COM、Slint 或具体 GPU 类型 |
| `crates/platform` | 平台服务接口、原子文件持久化、共享像素、Win32 辅助与平台能力查询 |
| `crates/render-win` | 按适配器捕获与旋转、GDI 抓屏降级、D3D11/WARP 设备、Direct2D/DirectWrite 标注缓存、GPU 马赛克与放大镜、导出回读、HDR→SDR 色彩管理、GPU 时间戳观测、DirectComposition 交换链与持久贴图 surface、命名 GPU 共享纹理 |
| `crates/ui` | Slint 软件渲染的工具栏与设置，只由编辑器加载；通过命令与编辑器通信 |
| `crates/core` | 事件驱动的托盘/快捷键循环与子进程生命周期 |
| `crates/editor`、`crates/pins` | 独立原生窗口线程；静态贴图管理器 |
| `crates/plugin-api`、`crates/plugin-host`、`plugins/example` | 版本化 C ABI、隔离的动态库宿主、示例诊断插件 |

---

## 已知限制与后续计划

- **仅 Windows 后端**；macOS 与 Linux 后端未实现，首版 Windows 优先的策略不变。
- **选区限制在单个显示器内**，未实现跨屏选区。
- **HDR 实机未验收**：HDR→SDR 转换链只有 GPU 单测（浮点 scRGB 亮度顺序与不透明 Alpha），开发机只有一块 SDR 60Hz 输出（测量时为 3840×2160 / 150% 缩放，目前为 2560×1440 / 100%），缺少真实 HDR/PQ 输入的端到端证据；8 位输入走 HDR 路径会被拒绝。
- **120Hz 与输入延迟未验收**：Present 等待与 GPU 时间戳被 60Hz 垂直同步钳制在约 16ms，无法分离真实 GPU 工作耗时；正式验收需要 120/144Hz 显示器与记录的负载。
- **跨进程穿透只有 API 级检查**：置顶、不抢焦点、透明区域命中等真实跨进程输入尚未验收。
- **设备与会话恢复未实机触发**：重建路径已由 smoke 直接调用验证（重建前后导出像素逐字节一致），但真实 `DXGI_ERROR_DEVICE_REMOVED`、锁屏/解锁与捕获访问失效没有实机证据；多屏、混合 DPI、旋转与热插拔同样缺硬件覆盖。
- **贴图缓存优化待做**：CPU 像素按需读取并释放，尚未接入领域 LRU、可见性/遮挡判定与显存预算。
- **未实现的功能不会假装可用**：OCR 插件、长截图插件与录屏引擎都还没有接入实际引擎，设置里的对应页面是明确的占位说明；当前只有能力位、接口与架构边界。
- **GPU 共享纹理还没有产品路径的生产方**：跨进程传输已实现并验证，但编辑器尚未调用插件，示例插件回读纹理只是为了算校验和。
- **窗口识别只实现了 UI Automation 模式**，像素识别未实现（配置枚举已预留）；识别依赖目标程序暴露 UI Automation 元素，桌面、任务栏和部分自绘界面不会高亮；查询内联在覆盖层线程上，提供者无响应时覆盖层会阻塞到 UI Automation 超时。

---

## 文档

- [插件 ABI 与所有权契约](docs/plugin-abi.md)
- [性能测量方法与限制](docs/performance.md)
- [开发计划与任务进度](开发计划.md)
- [仓库开发约定](AGENTS.md)
- [技术架构方案](Snaplight%20技术架构方案.md)

---

## 反馈与参与

- **问题与建议**：请在 [Issues](https://github.com/llight24/Snaplight/issues) 中提出，尽量附上构建类型、硬件、复现步骤与实际结果。
- **开发约定**：改动前请先读 [AGENTS.md](AGENTS.md)；计划、实测数字与验证结论统一维护在 [开发计划.md](开发计划.md)。
- **支持**：给本仓库点一个 [Star](https://github.com/llight24/Snaplight)，或把上面已知的验收缺口补齐（HDR、多屏、120Hz、真实跨进程输入）。

---

## 许可

Snaplight 以 **MIT** 许可发布，完整条款见 [LICENSE](LICENSE)；`Cargo.toml` 的 `workspace.package` 与各 crate 也声明了 `license = "MIT"`。
