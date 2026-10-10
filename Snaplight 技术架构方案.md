# Snaplight 技术架构方案

## 1. 核心决策与资源目标

推荐 **Rust 核心 + Windows 原生窗口/合成层 + Direct2D 画布 + Slint 面板 + 独立插件宿主**。

Slint 可以继续使用，但让它负责工具栏、设置和属性编辑；截图画布的绘制、输入、透明与穿透由专属模块控制。

按已确认的偏好，首版 Windows 优先，CPU 内存与显存分别统计，贴图数量和分辨率不设硬限制。因此：

- 无贴图待机以低于 30MB 为硬性验收目标。
- 持续贴图以低于 30MB 为典型负载目标，默认用五张 1080p 静态贴图验收；任意数量和尺寸无法保证固定上限。
- 所有自有常驻进程的私有工作集、私有提交分别汇总，不能通过拆进程隐藏开销；共享内存、显存另外报告。
- “CPU 0%”定义为无持续轮询、无周期绘制，静止状态平均 CPU 接近零；系统事件仍可能产生短暂活动。

单张 4K RGBA 图像约占 31.6MiB，截图底图、交换链、缓存必须单独预算。以下资源目标均需原型实测，不能由框架选型直接保证。

## 2. 分层与生命周期

```text
Rust 常驻核心：托盘、快捷键、配置、会话调度
 ├─ 按需编辑进程：截图覆盖层 + 画布 + Slint 工具栏/设置
 ├─ 按需贴图进程：原生窗口 + 静态 GPU 图像
 └─ 按需插件宿主：OCR / 长截图 / 录屏动态库
```

- **常驻核心**：使用 `windows` crate 接入 Win32，阻塞式消息循环；不初始化 UI、GPU、OCR 模型和录屏线程。
- **编辑进程**：首次截图时启动；最后一个编辑或设置窗口关闭后退出，用进程生命周期回收 UI、驱动及分配器缓存。冷启动耗时纳入验收。
- **贴图进程**：统一管理所有贴图，共享设备；静态内容提交给合成器后停止应用绘制，只在内容或显示状态改变时更新。移动、缩放和透明度优先使用合成变换。
- **图片存储**：原件与标注文档落盘，CPU 解码缓存采用有界 LRU；不可见贴图释放可重建纹理。可见贴图仍会随总像素数增加资源占用。
- **Slint UI**：使用独立的关联窗口，通过命令更新工具状态；主题、工具顺序和属性面板可配置。首版不共享 Slint 与画布的 GPU 上下文，避免绑定不稳定的互操作接口。[Slint 后端说明](https://docs.slint.dev/latest/docs/slint/guide/backends-and-renderers/backends_and_renderers/)

## 3. 画布选型与渲染路径

| 方案                   | 建议用途               | 判断                                               |
| ---------------------- | ---------------------- | -------------------------------------------------- |
| Direct2D + DirectWrite | Windows 首版画布与文字 | 首选，配合 D3D11、DXGI、DirectComposition          |
| Skia GPU               | 后续跨平台画布         | 成熟的路径、文字和图像能力；引入构建及运行资源成本 |
| tiny-skia              | 软件降级、局部栅格化   | CPU-only，缺少文字渲染，不作为 4K 高频交互主后端   |
| wgpu                   | 后续自定义 GPU 特效    | 提供 GPU 抽象，仍需另外实现或接入矢量、文字引擎    |

这些方案位于不同层级：Skia 是绘图引擎，wgpu 是 GPU API；二者都不能替代平台窗口和输入适配。[Skia 文档](https://skia.org/docs/) · [tiny-skia](https://github.com/linebender/tiny-skia) · [wgpu](https://github.com/gfx-rs/wgpu)

**Windows 主路径**采用 D3D11 设备、Direct2D device context、预乘 Alpha 的 DXGI composition swap chain，以及 DirectComposition。静态贴图优先使用持久合成 surface，避免每张贴图都保留高频交换链。[Direct2D 设备上下文](https://learn.microsoft.com/en-us/windows/win32/direct2d/devices-and-device-contexts) · [透明合成方案](https://learn.microsoft.com/en-us/archive/msdn-magazine/2014/june/windows-with-c-high-performance-window-layering-using-the-windows-composition-engine)

- 截图采用 Desktop Duplication，取得图像后复制到自有纹理，再显示覆盖层；处理多适配器、屏幕旋转和捕获失效。其输出本身是 DXGI surface，可避免正常交互路径中的 CPU 读回。[捕获 API](https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/desktop-dup-api)
- 保留不可变截图底图；标注采用独立文档模型与命令式撤销/重做，缓存已完成图元，当前笔画、选区与放大镜作为动态层。
- 马赛克读取底图，按图像坐标固定采样网格；缓存结果并保留遮罩操作，避免每帧全屏 CPU 处理。放大镜直接采样底图纹理。
- 帧调度采用“脏状态 + 显示节奏”：交互时按屏幕刷新率提交，无变化时休眠；合并选区移动事件，保留自由笔画采样点。
- 透明、命中测试、是否激活和整窗穿透分别建模。不能仅设置 `WS_EX_TRANSPARENT` 就认为完成穿透；`HTTRANSPARENT` 也有同线程限制。跨进程穿透必须用原生窗口路径验证，必要时为静态穿透模式采用 layered-window 降级。[窗口样式](https://learn.microsoft.com/en-us/windows/win32/winmsg/extended-window-styles) · [命中测试](https://learn.microsoft.com/en-us/windows/win32/inputdev/wm-nchittest)

## 4. 跨平台接口与插件边界

Cargo workspace 按核心领域、平台服务、渲染器、UI、插件 SDK 和宿主拆分。领域模型不依赖 HWND、COM、Slint 或具体 GPU 类型。

- **平台接口**：`CaptureService`、`WindowService`、`HotkeyService`、`ClipboardService`；能力查询明确表达全局快捷键、定位、置顶、穿透和捕获支持情况。
- **渲染接口**：`CanvasRenderer` 接收标注文档、视口、动态交互状态和图像资源 ID；设备、纹理、同步及呈现留在后端内部。
- **坐标与图像契约**：区分屏幕物理像素、窗口逻辑坐标、图像像素；图像携带尺寸、stride、像素格式、Alpha 和色彩空间。首版编辑统一 SDR，HDR 捕获转换到 SDR 并明确标识。
- **后续平台**：macOS 使用原生窗口与 ScreenCaptureKit，画布接 Skia/Metal；Linux 分开实现 X11、Wayland。Wayland 捕获通过 portal，并根据合成器能力提供功能，不能承诺完整复刻 Windows 行为。[ScreenCaptureKit](https://developer.apple.com/documentation/screencapturekit) · [Screenshot portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.Screenshot.html)
- **插件 ABI**：动态库使用 `cdylib`、`extern "C"`、`#[repr(C)]` 版本化函数表；禁止跨边界传递 Rust trait、`String`、`Vec` 或原生 GPU 对象。定义句柄、错误码、取消、线程约束及成对释放函数，panic 不得越过 FFI。[Rust ABI 文档](https://doc.rust-lang.org/reference/items/external-blocks.html)
- **插件运行**：动态库加载到按需宿主进程，任务结束后宿主退出。控制消息使用版本化 IPC，大图使用共享内存；平台 GPU 共享句柄作为可选优化。OCR 返回文字和坐标，长截图返回图像，录屏通过平台捕获服务获得帧流。

## 5. 验证与实施顺序

先做性能原型，再扩展完整产品；当前目录尚无实现代码。

1. **窗口原型**：验证透明合成、跨进程穿透、键鼠捕获、多屏混合 DPI、置顶和不抢焦点。
2. **画布原型**：在记录型号的 120Hz/144Hz 设备上测试 4K 选区、放大镜、马赛克和复杂标注；120Hz 的单帧预算为 8.33ms，分别记录 CPU/GPU 耗时、呈现间隔及输入延迟。
3. **资源原型**：验证无贴图及五张 1080p 静态贴图的待机目标；另测两张 4K、大量贴图、反复打开关闭编辑器，报告 CPU 内存、共享内存、显存和回收结果。
4. **正确性与恢复**：覆盖撤销重做、缩放后导出、混合 DPI 坐标、锁屏恢复、显示器热插拔、设备丢失，以及插件版本不兼容、取消、崩溃和缓冲区释放。

默认支持 Windows 10/11，跨平台后端随后加入；GPU 故障时允许软件降级并降低帧率。120Hz+ 是指定硬件和负载下的验收指标，静止画面不持续刷新。