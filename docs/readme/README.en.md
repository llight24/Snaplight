<h1 align="center">
  <a href="https://github.com/llight24/Snaplight"><img src="../../icon/snaplight-logo-horizontal.svg" alt="Snaplight" width="380" /></a>
</h1>

<p align="center">
  <a href="../../LICENSE"><img src="https://img.shields.io/badge/license-MIT-08C?style=flat" alt="License: MIT" /></a>
  <img src="https://img.shields.io/badge/platform-Windows%2010%20%7C%2011-4493F8?style=flat-square" alt="Platform: Windows 10 / 11" />
  <img src="https://img.shields.io/badge/Rust-2021-000000?style=flat-square&amp;logo=rust&amp;logoColor=white" alt="Rust 2021" />
  <img src="https://img.shields.io/badge/UI-Slint-4A86CF?style=flat-square" alt="UI: Slint" />
</p>

<p align="center">
  <sub><a href="../../README.md">中文</a> · <a href="README.en.md">English</a></sub>
</p>

<p align="center">
  <strong>A Windows-first screenshot, annotation and pinning tool.</strong><br/>
  A native Direct2D / DirectComposition canvas with Slint for the toolbar and settings; it stays in the tray and blocks on events instead of polling when idle.
</p>

---

## Features

### Screen capture

- **Per-adapter desktop capture**: Desktop Duplication grabs each monitor and copies the result into Snaplight's own texture. Frames that report no desktop update are skipped, waiting up to 1.5 seconds.
- **Explicit fallbacks**: if Desktop Duplication is unavailable or fails, capture falls back to GDI; if the hardware D3D11 device cannot be created, the WARP software rasterizer is used.
- **Frozen base image and immutable original**: the base image is frozen before the overlay appears, and annotations live in the document model only. Original pixels are never overwritten, so undo/redo always works and exports never accumulate quality loss.
- **HDR → SDR**: high-bit-depth input goes through colour-space conversion and tone mapping to sRGB output, and the image metadata is marked HDR→SDR.
- **Bounded failure handling**: capture retries a fixed number of times (3 attempts, 300ms apart) and then reports a clear error instead of retrying forever. On device loss or `D2DERR_RECREATE_TARGET`, the textures, swap chain and composition target are rebuilt from the retained base image and editing continues; `WM_DISPLAYCHANGE` rebuilds the same way instead of ending the session.

### Selection and crop

- Drag to draw a selection; everything outside it is dimmed to 55% so the region is easy to confirm.
- Selection chrome: blue outline (`#3388ff`, 2.2 logical pixels), eight round handles (white ring, blue core, 13 logical pixels outer / 9.2 inner), and a dark rounded chip showing the origin and size in screen pixels (`x,y  W × H px`). Every size scales with the monitor DPI.
- **Resize the crop with eight anchors**: corners change both axes, edge midpoints change one axis, the opposite edges stay fixed. The region is clamped to the image, never smaller than 1×1 pixel, and stops at the minimum instead of flipping when dragged past the opposite edge. One completed drag produces exactly one undoable history entry, and hovering shows the matching resize cursor.
- **Move the selection by dragging inside it**: a drag that starts inside the region moves the whole selection without changing its size, stopping at the image edges instead of being clipped or redrawn in reverse. Dragging outside the region is still what draws a new one, and the cursor inside shows a four-way move arrow.
- **Magnifier**: a 20×20 source area near the cursor shown at 6× with a white border and crosshair, clamped near the image edges.
- While a new selection is being drawn, the toolbar and quick options hide themselves; afterwards they are repositioned below the selection, or above it when there is no room, always clamped to the captured area. Undoing the last selection hides the toolbar, and redoing brings it back.
- Selections are currently limited to a single monitor.

### Annotation tools

| Tool | Description |
| --- | --- |
| Select | Draw a selection, move it by dragging inside it, and resize it with the eight anchors |
| Rectangle | Drag to draw a rectangle with adjustable stroke width and colour |
| Ellipse | Drag to draw an ellipse with adjustable stroke width and colour |
| Arrow | Draw an arrow from start to end point |
| Pen | Freehand stroke, drawn segment by segment |
| Mosaic | Pixelate with a block size of 12 by default |
| Text | Type in the quick options, then click the canvas to place it |

- **Quick options per drawing tool**: open them from the drop-down arrow on the right of a tool to set stroke width, colour (red/green/yellow/white), mosaic block size or the text. The panel is placed above or below the toolbar and clamped to the captured area.
- **White pill toolbar**: every icon is drawn as a Slint `Path` vector on a shared 24×24 grid with 1.75px strokes and rounded caps. It has a drag grip, group separators and tool drop-down arrows, and undo/redo are greyed out while there is nothing to reverse.
- **Self-sizing width**: the bar width is derived from the drawing tools in `Config.tools` and the action list, with stable command ids decoupled from display order, so adding, removing or reordering buttons needs no editor command changes. The bar hides while drawing and only repaints on interaction, with no resident frame loop.
- **Long capture**: select a region and click the long-capture button (command id 17) directly left of the pin command. A draggable frame appears around the selection with a bird's-eye view of the growing image on the right; put the pointer inside the selection and scroll the content with the wheel. Every time the picture comes to rest (two identical samples in a row, about 0.24 seconds) that screen is stitched in and the bird's-eye view updates. Once scrolling stops you can drag the top or bottom edge to cut the spare part of the last screen (the bottom edge after scrolling down, the top edge after scrolling up, at most one screen), or just keep scrolling. `Esc` or the panel's "cancel" button gives up without saving; `Enter` or "finish" saves the result into the capture directory as the usual triple (`<timestamp>.source.bgra`, `.document.json`, `.png`) and opens it as the current canvas right away, where the wheel pans, `Shift+wheel` pans sideways and `Ctrl+wheel` zooms around the pointer. Scrolling too fast for two screens to overlap (a jump) or reaching the height/memory limit stops the stitch and explains why in the panel, while the part already stitched stays usable. Snaplight never takes over the pointer and injects no input during a long capture, and the overlay and toolbar move out of the way, so the application cannot freeze.
- Annotations are drawn on the GPU (Direct2D) and cached as static layers, so only what changed is redrawn; mosaic and the magnifier also run on the GPU.

### Editing, export and clipboard

| Shortcut | Action |
| --- | --- |
| `Ctrl+Z` / `Ctrl+Y` | Undo / redo (selection resizes included) |
| `Ctrl+C` | Copy the selection to the clipboard (DIB) |
| `Ctrl+S` | Save a PNG |
| `Enter` | Pin the selection |
| `Esc` / right mouse button | Cancel this capture |

- **Complete on-disk captures**: each save writes `<timestamp>.source.bgra` (the original BGRA pixels), `<timestamp>.document.json` (a versioned document with the selection and every annotation) and either `<timestamp>.png` or the pin image, so a saved capture can be loaded and edited again instead of being just a flat export.
- Export uses GPU readback, with a GDI fallback path where needed.

### Pins

- **One process manages them all**: `snaplight-pins.exe` owns every pin window and exits once the last pin is closed, leaving no resident window process behind.
- **Position and size are preserved**: a new pin opens exactly where its region was captured (the editor sends the screen-physical origin of the region with the command), and only a region wider than the primary display is scaled down.
- **Interaction**: drag to move; mouse wheel scales (1.1× per notch); `Ctrl+wheel` changes opacity (5% per notch, clamped to 10%–100%); double-click restores 100%.
- **Context menu**: a white rounded card drawn by the same component as the tray menu: click-through (with the restore shortcut as a grey hint on the right, `Ctrl+Shift+P` by default; when a separate held-interaction key is configured the label also reads "hold Ctrl to interact") / divider / destroy (red, closes this one pin). Click-through mode always keeps a global way back to an interactive pin.
- **Hold to interact temporarily**: while a pin is click-through, holding the "held-interaction key" (default `Ctrl`, see [Settings window](#settings-window-chinese-first-ui)) restores mouse interaction for that pin and releasing it returns to click-through immediately, without waiting for the next poll. It differs from "restore pins", which toggles once and does not return to click-through by itself. The key may be a single modifier, a modifier combination or a single ordinary key, so it is **not registered as a global hotkey**; instead the key state is polled every 16 ms only while at least one click-through pin exists, and the timer is closed again when there is none, so an idle process never polls. If the key value is unusable, the pin process only disables that detection and prints the reason, and already-open pins keep their management entry points.
- **No focus stealing**: pin windows answer mouse activation with `MA_NOACTIVATE`, so clicking a pin does not take focus away from the current application.
- **Two backends**: static pins use layered windows by default (low private memory); enabling "GPU 贴图" in settings switches them to persistent DirectComposition surfaces. GPU pins reset scale to 100% when they enter click-through mode.
- **Click-through relies on layered hit testing**: mouse click-through sets both `WS_EX_LAYERED` and `WS_EX_TRANSPARENT` — setting the transparent bit alone does not remove the window from hit testing. A GPU pin therefore has to fall back to layered bitmap rendering while it is click-through (its no-redirection bitmap style cannot be cleared once the window is created) and rebuild its composition target when it becomes interactive again; DirectComposition returns `DCOMPOSITION_ERROR_WINDOW_ALREADY_COMPOSED` while the old target is still being released, so the rebuild retries instead of failing on a single switch.
- Textures are released when a pin is hidden, but not yet reclaimed on compositor occlusion (see [Known limitations](#known-limitations-and-roadmap)).

### UI element detection under the cursor

- With window detection set to the UI Automation mode, the select tool **highlights the UI Automation element under the cursor** while nothing is selected: outline only, with no handles and no size chip, so it is never mistaken for a finished selection. A left click adopts exactly that region.
- Manual dragging still works. With detection on, a click that finds no element selects nothing; with detection off, the previous behaviour is unchanged.
- Element chain strategy: walk up to 10 levels from the element under the cursor to the first `ControlType == Window` ancestor, then take the first element between it and the cursor whose rectangle is at least 8 physical pixels on both axes, falling back to the window rectangle.
- The desktop, the taskbar and windows that expose no UI Automation provider produce no highlight at all.
- It works by punching a temporary 1×1 hole in the overlay window region so UI Automation can hit-test the element underneath, restoring the region immediately after the query. The query runs inline on the overlay thread (measured 5–33ms), throttled to ≥3 physical pixels and ≥33ms while moving, and a queued probe point is updated to the newest position.
- The setting is read when a capture session starts, so a change applies to the next capture. The mode is an enum, leaving room for a pixel-based mode without changing the stored configuration.

### Resident core and global hotkeys

- `snaplight-core.exe` stays in the notification area and runs nothing but a blocking Win32 message loop: it **does not initialise Slint, the GPU, OCR models or a recording thread**.
- The tray context menu is a self-drawn white rounded card that ignores the system theme: capture (with the current hotkey as a grey hint on the right) / settings / close all pins (red) / divider / exit. It opens at the pointer's **upper right** (the card's bottom edge sits on the pointer and it grows to the right; it drops below the pointer when there is no room above) and is clamped into that monitor's work area, and the mouse, Esc, ↑/↓ and Enter all work.
- Global hotkeys are configurable in `Ctrl+Shift+A` form (case-insensitive, modifiers in any order, at least one modifier, supporting A–Z, 0–9 and F1–F24): `Ctrl+Shift+A` starts a capture and `Ctrl+Shift+P` restores interaction for every click-through pin by default. A hotkey already taken by another program falls back to the default and prints the reason; an unparseable hotkey is rejected while the configuration is loaded. The pin "held-interaction key" (default `Ctrl`) is not a global hotkey: it accepts a single modifier or a single ordinary key and is polled on demand by the pin process (see [Pins](#pins)).
- Child processes start on demand and are reclaimed: the editor exits with its capture session, the pin manager exits after the last pin closes, and one plugin host serves one task. Cleanup uses the retained process handle, so a recycled PID cannot kill an unrelated process.

### Settings window (Chinese-first UI)

The settings window is a left navigation pane plus right-hand options, with seven pages:

| Page | Contents |
| --- | --- |
| 常规 (General) | GPU pin backend; drawing tool order with per-tool up/down moves (stable tool ids decoupled from display order) |
| 外观 (Appearance) | Theme background, foreground and accent colours |
| 快捷键 (Hotkeys) | Capture and pin-restore global hotkeys, plus the pin held-interaction key (default `Ctrl`; a single modifier, a modifier combination or a single ordinary key; an empty or unparseable value is rejected with a prompt and the canonical spelling is written back; when it equals the restore hotkey the pin menu stops repeating the hint) |
| 截图 (Capture) | Drawing defaults (stroke width, mosaic block), window detection mode, capture folder (typed or chosen with the shell folder picker), and the saved captures in that folder (local time / format / size, with open, open folder and delete-to-recycle-bin; deletion needs a second confirming click) |
| 翻译 (Translation) | Placeholder page that states the feature is not implemented yet |
| 屏幕录制 (Screen recording) | Placeholder page that states the feature is not implemented yet |
| 设置 (Settings) | App version, configuration file path, data directory and a button that opens it |

- The toolbar quick options, tray menu, pin context menu and settings pages are all **Chinese-first**; code identifiers, commands and error codes stay in their original form.
- Accessibility: Slint exposes an accessibility tree (labels, buttons, edits, check boxes) through the `accessibility` feature, so Windows UI Automation and screen readers can enumerate and drive the settings page and toolbar using the same Chinese labels users see.
- Toolbar and settings window positions are remembered in `ui-state.json`; a missing, corrupt or version-mismatched file only falls back to default positions.
- Status messages state the outcome explicitly — saved, click "confirm delete" again, N files moved to the recycle bin, or an invalid hotkey format that wrote nothing.

### Configuration and data directory

- Configuration and captures live in `%LOCALAPPDATA%\Snaplight` by default; set `SNAPLIGHT_DATA_DIR` to override.
- `config.json` stores theme colours, stroke width, mosaic block size, tool order, window detection mode, the GPU pin switch, the capture folder, both global hotkeys and the pin held-interaction key; a configuration written before that field existed falls back to `Ctrl`. `ui-state.json` stores window positions.
- Captures default to `<data dir>\captures` and can be changed in settings.
- The 翻译 and 屏幕录制 pages are placeholders: **an unimplemented feature never gets a setting that pretends to work**.

### Plugins and extensibility

- Plugins use a versioned C ABI (`extern "C"` + `#[repr(C)]`); Rust collections, trait objects and native GPU objects never cross the boundary.
- A separate host process isolates faults: a crashing plugin does not take the resident core with it. Host isolation is fault isolation, not a security sandbox for untrusted plugins.
- Large images travel through read-only shared memory, control messages use a bounded, versioned JSON protocol, and named GPU shared textures are supported too (`SHARED_NTHANDLE | SHARED_KEYEDMUTEX` with a keyed-mutex handshake and bounded timeouts), verified across a real process boundary.
- The host validates transport metadata and ABI version/structure sizes before opening mappings or loading the library, and rejects requests for capabilities a plugin does not declare. Cancellation, timeouts, forced exit and library unload order all have explicit contracts. See the [plugin ABI contract](../plugin-abi.md).
- `plugins/example` is a **diagnostic inspect plugin** (it reads an image and computes a checksum). It is not an OCR engine and cannot be used to accept OCR as working.

### Performance and resources

Measurement environment: Windows, Intel Core i9-14900KF (32 logical CPUs), NVIDIA RTX 5060 Ti, 3840×2160 / 60Hz, release build, 5-second CPU sampling.

| Scenario | Total private working set | Total private commit | Total shared commit | CPU delta |
| --- | ---: | ---: | ---: | ---: |
| Tray idle, no pins | 1.07 MiB | 1.51 MiB | 9.02 MiB | 0 ms |
| Five 1920×1080 pins | 2.25 MiB | 3.29 MiB | 57.61 MiB | 0 ms |
| Two 3840×2160 pins | 2.25 MiB | 3.27 MiB | 49.68 MiB | 0 ms |
| Twenty 1920×1080 pins | 2.21 MiB | 3.42 MiB | 168.40 MiB | 0 ms |

- The accounting sums the private working set and private commit of **every resident Snaplight process**, with shared commit reported separately. Counting shared image buffers as system memory means sustained pins are not below 30 MB in total; layered windows do not make image memory or DWM GPU resources disappear, and pin count and resolution have no product-level hard limit.
- Stress mode with a real 4K base image, 1000 annotations, and the magnifier plus a 600-point live stroke redrawn every frame (release build, 241 frames): CPU submission p50 0.902–0.960ms, p95 1.48–1.58ms, p99 1.83–1.87ms, max 3.40–3.49ms — all within the 8.33ms budget of 120Hz.
- CPU submission time, Present wait and D3D11 GPU timestamps are three different metrics, recorded separately and never substituted for one another. Full methodology and limits are in the [performance report](../performance.md).

---

## Quick start

```powershell
.\scripts\launch.ps1            # release build + start the tray process + print a manual test checklist
.\scripts\launch.ps1 -SkipBuild # start without rebuilding
.\scripts\launch.ps1 -Settings  # open the settings window directly
.\scripts\launch.ps1 -Restart   # stop, rebuild, start again
.\scripts\launch.ps1 -Stop      # shut down every Snaplight process gracefully
```

Basic flow:

1. Click the tray icon, or press `Ctrl+Shift+A`.
2. Drag a region — or click a UI element highlighted by detection. Drag inside the region to move it, or drag the handles to resize the crop.
3. Pick a tool and draw. Use the drop-down arrow for stroke width, colour, mosaic block or text; type text first, then click the canvas to place it.
4. `Ctrl+Z` / `Ctrl+Y` undo and redo, `Ctrl+C` copies a DIB, `Ctrl+S` saves a PNG, `Enter` pins the selection, `Esc` cancels.
5. Drag a pin to move it, wheel to scale, `Ctrl+wheel` for opacity, right-click for its menu, and `Ctrl+Shift+P` to restore interaction for every click-through pin at once. While a pin is click-through, hold `Ctrl` (configurable in settings) to operate it and release to return to click-through.

This is a compiled native application: **there is no hot reload**, and Windows refuses to replace a running executable, so source edits only take effect after `-Stop` / `-Restart` and a rebuild.

---

## Install and build

Windows 10/11 and a Rust toolchain are required. With a standard Rust MSVC toolchain and the Windows SDK you can use Cargo directly:

```powershell
cargo build --workspace --locked
cargo test --workspace --locked
.\target\debug\snaplight-core.exe
```

The workspace also supports the local GNU/LLVM toolchain installed under the ignored `.tools` directory through `scripts/dev.ps1`:

```powershell
.\scripts\dev.ps1 build -Release
.\scripts\dev.ps1 test
.\scripts\dev.ps1 clippy
.\scripts\dev.ps1 fmt
.\scripts\dev.ps1 smoke -Release
.\scripts\dev.ps1 run -Release
```

Ship the four executables together: `snaplight-core.exe`, `snaplight-editor.exe`, `snaplight-pins.exe` and `snaplight-plugin-host.exe` (plus the plugin libraries under `plugins/`). The toolchain directory is machine-local and is not part of the repository.

All application and window icons live in `icon/`: `icon.ico` is generated from `icon.svg` with the sizes Windows asks for (16/24/32/48/64/128/256) and every executable embeds it as resource id 1 through `scripts/build/app_icon.rs`. The notification-area icon and the window `WM_SETICON` reuse that same resource, so replacing the icon means regenerating `icon.ico` and rebuilding.

---

## Development and verification

| Command | Purpose |
| --- | --- |
| `.\scripts\dev.ps1 build -Release` | Release build |
| `.\scripts\dev.ps1 test` | Build the workspace first, then run every test (the plugin-host integration tests load the example plugin library from the build directory) |
| `.\scripts\dev.ps1 clippy` | Clippy across all targets (`-D warnings`) |
| `.\scripts\dev.ps1 fmt` | Formatting check |
| `.\scripts\dev.ps1 smoke -Release` | End-to-end smoke: real capture, deterministic pixel checks, UI snapshots, both pin backends, the example plugin |
| `.\scripts\resource-scenarios.ps1 -Profile release -Seconds 10` | Resource scenarios: idle, five 1080p pins, two 4K pins, twenty pins |
| `.\scripts\measure-resources.ps1` | Aggregate private working set, private commit, shared commit, CPU deltas and available GPU counters for the executables in a build directory |

The smoke suite asserts pixels on a real capture (and compares it against an independent GDI grab of the same screen using sparse sampling, so an all-black base image cannot pass silently), saves one snapshot per settings page and for all three quick-option variants, renders the tray menu at 100/125/150/200% and asserts its colours and geometry, checks the toolbar's physical size against the monitor DPI, that its placement stays inside the captured area, that window positions round-trip through `ui-state.json`, and that a pin opens at the position it was given and keeps it while scaling. `snaplight-editor.exe --stress` renders 240 frames of a heavy workload on a real 4K base image and reports CPU submission, Present and GPU timestamp percentiles; `snaplight-core.exe --menu-demo [x y]` opens the real tray menu at the pointer's upper right (or at an explicit x/y anchor) and only prints the chosen entry without performing it, which makes the menu's appearance and placement easy to check by hand (it can coexist with a running tray instance).

It does **not** simulate physical cross-process mouse input, and it does not prove refresh-rate performance. Choose verification by risk: domain and protocol changes run the relevant tests, window/graphics/lifecycle changes run the release smoke, and performance changes re-measure the affected workload. Documentation-only changes just need to stay consistent with the implementation, commands and paths.

---

## Architecture

Process boundaries:

```text
snaplight-core.exe            resident tray: blocking Win32 message loop, tray menu, global hotkeys, child scheduling
 ├─ snaplight-editor.exe      on-demand capture: capture, annotation canvas, toolbar, settings window
 ├─ snaplight-pins.exe        manages every pin, exits when the last one closes
 └─ snaplight-plugin-host.exe one host process per task; a plugin crash cannot take down the resident core
```

Module boundaries:

| Crate | Responsibility |
| --- | --- |
| `crates/domain` | Coordinates, image contracts, documents, annotations, history, bounded LRU cache, event demand merging, configuration and versioned IPC; no Win32, COM, Slint or concrete GPU types |
| `crates/platform` | Platform service interfaces, atomic file storage, shared pixels, Win32 helpers and capability queries |
| `crates/render-win` | Per-adapter capture and rotation, GDI capture fallback, D3D11/WARP devices, Direct2D/DirectWrite annotation cache, GPU mosaic and magnifier, export readback, HDR→SDR colour management, GPU timestamp profiling, DirectComposition swap chains and persistent pin surfaces, named GPU shared textures |
| `crates/ui` | Slint software-rendered toolbar and settings, loaded only by the editor and driven by commands |
| `crates/core` | Event-driven tray/hotkey loop and child-process lifecycle |
| `crates/editor`, `crates/pins` | Independent native window threads; the static pin manager |
| `crates/plugin-api`, `crates/plugin-host`, `plugins/example` | Versioned C ABI, isolated dynamic-library host, example diagnostic plugin |

---

## Known limitations and roadmap

- **Windows backend only**; the macOS and Linux backends are unimplemented, and Windows-first remains the plan.
- **Selections are limited to a single monitor**; cross-monitor selections are not implemented.
- **HDR is not hardware-verified**: the HDR→SDR chain only has GPU unit coverage (floating-point scRGB luminance ordering and opaque alpha). The development machine has a single SDR 60Hz output (3840×2160 at 150% scaling when measured, 2560×1440 at 100% today), so there is no end-to-end evidence from a real HDR/PQ input; 8-bit-only input is rejected on the HDR path.
- **120Hz and input latency are not verified**: Present wait and GPU timestamps are pinned near 16ms by 60Hz vsync, so real GPU work cannot be separated. Formal acceptance needs a 120/144Hz display with a recorded workload.
- **Cross-process click-through now has a hit test but still lacks human acceptance**: the smoke first asserts that a click-through window carries `WS_EX_TRANSPARENT`, then uses the system's own `WindowFromPoint` at the window centre to confirm the window is no longer part of the hit result, for both the layered and the GPU backend. The end-to-end manual check — that holding a real key keeps the mouse reaching the window underneath (on multiple monitors and mixed DPI) — is still outstanding.
- **Device and session recovery has not been triggered on real hardware**: the rebuild path is exercised directly by the smoke suite (exported pixels are byte-identical before and after a rebuild), but a real `DXGI_ERROR_DEVICE_REMOVED`, lock/unlock and capture-access loss have no hardware evidence, and multi-monitor, mixed-DPI, rotation and hot-plug coverage is missing for the same reason.
- **Pin cache optimisation is pending**: CPU pixels are read on demand and released, with no domain LRU, visibility/occlusion decision or VRAM budget yet.
- **Unimplemented features never pretend to work**: the OCR plugin and the recording engine have no real engine behind them, and the matching settings pages are explicit placeholders; today there are only capability bits, interfaces and architectural boundaries.
- **Long capture is now "scroll it yourself, it stitches when it rests", accepted on real hardware with known limits**: unit tests cover the stitcher and the session logic (content advancing, a sticky header, nothing moving, a jump, the memory and height limits, edge trimming, panel placement), and a scrollable test window has driven the full "real wheel → stitch on rest → trim an edge → finish → save → open as canvas" path: 10 wheel notches of 120 pixels each stitched 11 screens into a 700 × 1760 image whose 45 test bands were checked row by row — all present, consecutive, 40 rows each (only the first is 10 rows short because the selection starts 10 rows below the top of the content). Known limits: the wheel goes to the window under the pointer, so the selection has to cover scrollable content; stitching only happens once the picture rests, so a single scroll of more than one screen (no overlap left between two screens) is reported as a jump and stops the stitch, while the part already stitched stays usable; GDI capture applies one tone-mapping difference on HDR screens and HDR is not accepted (this machine is a single 3840×2160 / 150% SDR output).
- **No product path produces GPU shared textures yet**: the cross-process transport is implemented and verified, but the editor does not invoke plugins, and the example plugin reads the texture back only to compute a checksum.
- **Window detection only implements the UI Automation mode**; a pixel-based mode is not implemented (the configuration enum already reserves room). Detection depends on the target application exposing a UI Automation element, so the desktop, the taskbar and some custom-drawn surfaces report nothing, and the inline query blocks the overlay until UI Automation times out when a provider stops responding.
- **The tray and pin menus expose no UI Automation information**: to draw the reference white rounded card, both context menus are painted directly by a native layered window, so a screen reader cannot enumerate their entries (the previous system menus could). Mouse and keyboard operation (Esc, ↑/↓, Enter) is unaffected.

---

## Documentation

- [Plugin ABI and ownership contracts](../plugin-abi.md)
- [Performance measurement methodology and limits](../performance.md)
- [Development plan and task progress](../../开发计划.md) (Chinese)
- [Repository development conventions](../../AGENTS.md) (Chinese)
- [Technical architecture proposal](../../Snaplight%20技术架构方案.md) (Chinese)

---

## Community and support

- **Questions and ideas**: open an [issue](https://github.com/llight24/Snaplight/issues) and include the build type, hardware, reproduction steps and observed result where you can.
- **Contributing**: read [AGENTS.md](../../AGENTS.md) first; plans, measured numbers and verification conclusions are kept in the [development plan](../../开发计划.md).
- **Support**: [star](https://github.com/llight24/Snaplight) the repository, or help close one of the acceptance gaps above (HDR, multi-monitor, 120Hz, real cross-process input).

---

## License

Snaplight is released under the **MIT** license; the full text is in [LICENSE](../../LICENSE), and `Cargo.toml` declares `license = "MIT"` for the workspace and its crates.
