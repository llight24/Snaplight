# 模块职责与接口

任务状态和验证记录维护在 `开发计划.md`；本文件说明源码职责及调用方式。

## crate 依赖方向

| crate | 允许的内部产品依赖 |
| --- | --- |
| domain | 无 |
| platform | domain |
| render-win | domain、platform |
| ui | domain |
| core | domain、platform、plugin-api |
| editor | domain、platform、render-win、ui |
| pins | domain、platform、render-win |
| plugin-api | domain |
| plugin-host | domain、platform、plugin-api |
| example-plugin | domain、plugin-api |

`plugin-host` 的 GPU 传输集成测试可以通过 dev-dependency 使用 render-win，它不是宿主产品的图形依赖。

运行 `scripts/check-architecture.ps1` 检查 Cargo metadata 中的内部普通/build 依赖、目标条件、别名与未登记包，并阻止 domain 直接依赖原生/UI/GPU 包。它检查直接依赖声明，不能代替代码审查、Rust 可见性检查、第三方传递依赖分析或运行验证。

## 编辑器

| 模块 | 职责与状态所有权 |
| --- | --- |
| native | 启动参数、Slint 控件创建与命令接线、线程组合、UI smoke 调度 |
| canvas_session | 私有 Canvas、文档/历史/底图、交互状态、HWND 及渲染资源、会话线程与消息处理 |
| canvas_session 内的 scroll_capture | 画布会话内部的长截图实现；Canvas 不导出给协调入口 |
| ui_bridge | 弱引用注册、工具栏尺寸/位置与选区/工具缓存；字段与锁私有 |
| selection | 选区命中、移动与缩放几何；仅依赖领域值类型 |
| pin_edit | 贴图编辑参数和文件组、加载与写回；路径及标志私有 |
| output | PNG、剪贴板、截图源文件及贴图文件包输出 |
| tool_windows | 原生工具窗口定位、图标、圆角、subclass、置顶和窗口位置持久化 |
| settings | 设置控件回调、配置校验/保存、截图列表和目录选择 |
| smoke | 确定性样例、像素与窗口观测辅助；不持有业务会话状态 |

UI 线程创建控件并注册弱引用；画布线程处理命令和绘制。桥接缓存的比较与更新在一次加锁内完成，调用 UI 前释放锁。会话输入/输出 DTO 用于启动参数与就绪结果，不传递可修改的 Canvas 内部状态。

## 图形服务

| 模块 | 职责与资源所有权 |
| --- | --- |
| gpu | D3D11/D2D/DXGI/DirectWrite 设备及工厂，硬件与 WARP 初始化 |
| capture | Desktop Duplication、GDI 降级、冻结帧、捕获一致性检查；冻结帧拥有纹理与设备引用 |
| composition | 一个 HWND 的合成设备/目标/visual，以及静态贴图 surface |
| windows_backend | 编辑画布绘制、标注缓存、导出、设备重建与绘制观测 |
| text_outline | DirectWrite 回调与字形几何收集；返回拥有接口引用的几何列表 |
| d2d | 共享的矩形、变换及 BGRA bitmap 属性转换；Alpha 尊重调用参数 |
| gpu_share | 共享纹理发布、互斥键与共享句柄生命周期 |

根模块使用显式导出。GPU/冻结帧/合成资源字段私有，内部互操作接口只开放实际所需范围。底图冻结、目标复用、资源释放顺序、按需初始化及产品后端选择保持原实现。

## 平台与插件

- platform/storage 管理配置、窗口状态、像素文件和截图文件组，私有解析不对外导出。
- platform/native/shared_pixels 管理映射与地址，长度和名称只能查询，借用 bytes 不超出映射生命周期。
- platform/native/probe 管理 UI Automation、探测策略和临时窗口区域 guard，COM apartment 与 GDI 区域按原所有权释放。
- plugin-host/main 负责启动、错误报告和退出；host 服务负责请求校验、加载、ABI 检查、执行、取消、结果回收和输出。
- pins 保持统一贴图会话管理，shadow 模块负责阴影几何/像素；渲染后端通过 render-win 公开服务协作。

## 字段访问迁移

以下是有意收紧的 Rust 源码 API；仓库调用方已同步，配置、IPC 与插件 ABI 未因此改变版本。

| 原字段 | 查询接口 |
| --- | --- |
| FrozenFrame.descriptor / screen | descriptor() / screen() |
| FrozenFrame.fallback / gpu.software | is_fallback() / is_software() |
| OverlayRenderer.frames | frame_count() |
| OverlayRenderer.cpu_us / present_us / gpu_us | cpu_samples_us() / present_samples_us() / gpu_samples_us() |
| StaticSurface.width / height | size() |
| SharedPixels.len / name | len() / name()；另提供 is_empty() |

Gpu 的设备句柄字段不再供外部直接访问；构造、帧上传、绘制与共享纹理通过现有服务 API 完成。观察数据返回只读切片，不允许调用方改写采样缓冲或计数。
