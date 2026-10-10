# Snaplight

Windows-first screenshot and pinning prototype in Rust. Native Direct2D and
DirectComposition own the canvas; Slint owns the separate toolbar and settings.

## Build and Run

With a standard Rust MSVC toolchain and Windows SDK:

```powershell
cargo build --workspace --locked
cargo test --workspace --locked
.\target\debug\snaplight-core.exe
```

This workspace also supports the local GNU/LLVM tools installed under the
ignored `.tools` directory through `scripts/dev.ps1`:

```powershell
.\scripts\dev.ps1 build
.\scripts\dev.ps1 test
.\scripts\dev.ps1 smoke
.\scripts\dev.ps1 build -Release
.\scripts\dev.ps1 run -Release
```

For hands-on testing there is a single entry point that builds (Release by
default), starts the resident tray process, prints a manual test checklist, and
returns while the app keeps running:

```powershell
.\scripts\launch.ps1                  # build + start, then test the tray/hotkey flow
.\scripts\launch.ps1 -SkipBuild       # start without rebuilding
.\scripts\launch.ps1 -Restart         # stop, rebuild, start again (no hot reload exists)
.\scripts\launch.ps1 -Settings        # open the editor settings window directly
.\scripts\launch.ps1 -Stop            # graceful shutdown of every Snaplight process
```

This is a compiled native app: there is no hot reload, and Windows refuses to
replace a running executable, so source edits only take effect after
`-Stop`/`-Restart` followed by a rebuild.

The toolchain directory is machine-local and is not part of the repository.
Ship the four executables together: core, editor, pins, and plugin-host.

Click the tray icon or press Ctrl+Shift+A to capture. Drag a selection, choose
a tool, and draw. Enter pins the selected area; Escape cancels. Ctrl+Z/Y undo
and redo, Ctrl+C copies a DIB, and Ctrl+S saves PNG. Type text in the toolbar,
then click the canvas to place it. The toolbar opens inside the captured desktop
(bottom-left) unless it has a remembered position, moves next to the selection
when one is completed, and shows the selection size in pixels. Right-click the
tray for settings and exit.
Standalone editor pin commands write IPC to stdout and need the core to show
the resulting pin.

Drag a pin to move it. A new pin opens exactly where its region was captured,
so pinning does not shift the image; only a region wider than the primary
display is scaled down. Mouse wheel scales; Ctrl+wheel changes opacity.
Right-click a pin to enable click-through, reset size, or close. Ctrl+Shift+P
restores interaction for all click-through pins. Static pins default to layered
windows to avoid retaining an application graphics driver context. Enable
"GPU pins" in settings to use persistent DirectComposition surfaces. GPU pins
reset scale to 100% when entering click-through mode.

Configuration and saved captures live in `%LOCALAPPDATA%\Snaplight`; set
`SNAPLIGHT_DATA_DIR` to override. Each saved capture includes original
`*.source.bgra` pixels and a versioned `*.document.json`, plus a PNG or flattened
pin image. Theme colors, stroke width, mosaic size, and tool order are stored in
`config.json`; the settings window edits colors, drawing defaults, the pin
backend, the tool order (Up/Down per tool), the capture folder (typed or
chosen with the shell folder picker), and the two global shortcuts (capture and
pin restore, written in `Ctrl+Shift+A` form). It also lists the saved captures
in that folder (local time, PNG or BGRA, total size) with Open, Open folder, and
Delete; Delete needs a second confirming click and moves the capture's files to
the recycle bin. Captures default to
`<data>\captures`. Shortcut and backend changes apply when the next core or pin
manager process starts; a shortcut that cannot be registered falls back to the
default, and an unparseable one is rejected when the config is loaded. The
toolbar and settings window remember their positions in `ui-state.json`; a
missing or unreadable state file only falls back to default positions.

The Slint tool windows expose an accessibility tree (labels, buttons, spinners,
edits) through the `accessibility` feature, so Windows UI Automation and screen
readers can drive the settings page and toolbar.

## Architecture

- `domain`: typed coordinates, image metadata, annotation history, bounded cache,
  event demand, configuration, and versioned IPC.
- `platform`: service contracts, atomic storage, shared pixels, Win32 helpers.
- `render-win`: per-adapter capture, rotation, GDI capture fallback, D3D11/WARP,
  Direct2D cached annotations, GPU mosaic and magnifier, export readback,
  HDR-to-SDR color management, GPU timestamp profiling, composition swap chains,
  persistent pin surfaces, and named GPU shared-texture publishing for plugins.
- `ui`: Slint software-rendered toolbar/settings, loaded only by the editor.
- `core`: event-driven tray/hotkey loop and child-process lifecycle.
- `editor` and `pins`: independent native window threads and static pin manager.
- `plugin-api`, `plugin-host`, and `plugins/example`: C ABI, isolated DLL host,
  and a versioned request that carries either a read-only shared-memory mapping
  or a named GPU shared texture.

See [plugin ABI](docs/plugin-abi.md) for ownership and cancellation contracts.

## Verification and Limits

`scripts/dev.ps1 smoke` captures each monitor, renders annotations, exports PNG
and timing artifacts, exercises static composition and layered transitions, and
invokes the example plugin. Deterministic source/crop/mosaic pixel checks, a
toolbar rendering snapshot, a settings rendering snapshot, a window-position
round trip through `ui-state.json`, a toolbar placement check inside the
captured desktop, a DPI check of the toolbar's physical size against the
monitor DPI (`toolbar-window.json`), a check that a pin opens at the screen
position it was given (and keeps it while scaling), and a comparison of the real
captured frame against an independent GDI capture of the same screen
(`capture_agreement` in `smoke-1.json`), which catches an empty capture that the
synthetic-frame pixel checks cannot see. The settings
snapshot also
shows the saved-capture list built from two synthetic captures, and one of them
is deleted through the recycle-bin path to prove deletion removes the files.
`scripts/dev.ps1 test` builds the
workspace first, because the plugin-host integration tests load the example
plugin library; those tests also cover the GPU shared-texture transport across a
real process boundary. It does not simulate physical
cross-process mouse input or prove refresh-rate performance.
`scripts/measure-resources.ps1` sums
private working set, private commit, shared commit, CPU deltas, and available
GPU process counters across executables in the selected build directory.
`scripts/resource-scenarios.ps1` runs isolated idle, five-1080p, two-4K, and
twenty-pin scenarios and shuts down the test processes. `snaplight-editor.exe
--stress` captures the real desktop and renders 240 frames over 1000 annotations
with the magnifier and a live stroke redrawn every frame, reporting per-frame
CPU-submission, Present and GPU-timestamp percentiles. See
[measured results](docs/performance.md).

The 30 MB target includes all resident Snaplight processes. Unlimited visible
pins cannot have a fixed memory bound. Use no pins and five 1080p pins as separate
acceptance scenarios; also report two 4K pins and a high-count stress case.
120 Hz must be measured on a 120 Hz display with recorded hardware and workload.
CPU draw time, blocking Present time, and D3D11 GPU timestamp time are reported
separately. Shared memory and DWM resources are not made free by using layered
windows; the private-memory target does not describe total system footprint.

Current limitations: selections stay within one monitor; the current test machine
has a single SDR 60 Hz display at 150% scaling, so HDR capture, 120 Hz rendering
and mixed-DPI/multi-monitor acceptance are still outstanding, and cross-process
click-through has only API-level checks; actual HDR display
capture still needs hardware validation (the floating-point effect chain is
tested), and 8-bit-only HDR capture paths are rejected; lost D3D11 devices,
reported `D2DERR_RECREATE_TARGET`, and display configuration changes rebuild the
session from the retained base image instead of ending it (a real device-removal
event, locked sessions and capture-access loss are not yet hardware-verified),
and capture failures retry a bounded number of times before exiting with an
error; pin textures are released on hide but not by compositor occlusion; actual
OCR, long capture and recording engines and alternate platform backends remain
future work. The included inspection plugin is a diagnostic, not an OCR
substitute. The GPU shared-texture transport is implemented and verified across
processes, but no engine consumes it yet: the editor does not invoke plugins,
and the example reads the texture back to the CPU only to compute a checksum.
