# Performance Validation

Measured on 2026-10-10 with Windows, an Intel Core i9-14900KF (32 logical CPUs),
NVIDIA RTX 5060 Ti driver 32.0.16.1692, and a single 3840x2160 display at 60 Hz.
A DXGI probe reports the only active output as 3840x2160, rotation identity,
144 DPI (150% scaling), `DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709`, 8 bits per
color and about 417.7 nit maximum luminance: this is an SDR panel, so HDR and
120 Hz acceptance cannot be exercised here. Native builds used Rust 1.99.0 and
the local GNU/LLVM toolchain. Values below use MiB (1,048,576 bytes); the
original goal of 30 MB is 30,000,000 bytes.

## Release Resident Processes

Five-second CPU samples, with synthetic opaque images and the low-memory pin
backend. Both core and pin manager are included in each pin row.

| Scenario | Private working set | Private commit | Shared commit | CPU delta |
| --- | ---: | ---: | ---: | ---: |
| Tray idle | 1.07 MiB | 1.51 MiB | 9.02 MiB | 0 ms |
| Five 1920x1080 pins | 2.25 MiB | 3.29 MiB | 57.61 MiB | 0 ms |
| Two 3840x2160 pins | 2.25 MiB | 3.27 MiB | 49.68 MiB | 0 ms |
| Twenty 1920x1080 pins | 2.21 MiB | 3.42 MiB | 168.40 MiB | 0 ms |

The private-memory target passes in these short scenarios. Total CPU/system
memory is not below 30 MB when image buffers in shared memory are included.
Layered windows shift image storage to OS-managed resources; no app GPU memory
counter instance was reported in these runs. DWM GPU allocations are not
attributed to Snaplight by this script and are not zero. Longer soak and physical
input tests are still necessary for a product release.

An earlier five-pin run with DirectComposition measured 23.42 MiB private
working set, 84.86 MiB private commit, approximately 69.95 MiB app dedicated GPU
memory and 9.46 MiB app shared GPU memory. This failed the private-commit target,
which is why static pins now default to layered windows. GPU composition remains
available for users prioritizing transform performance.

## Rendering

The smoke harness checks real Desktop Duplication capture plus a deterministic
640x400 source texture. Duplication frames that report no desktop update
(`LastPresentTime` zero, which is what the first frame after `DuplicateOutput`
usually is) are released and skipped rather than copied, because copying one
yields an all-black capture; the capture waits up to 1.5 s for an update and then
uses the GDI fallback. The harness compares the real captured frame against an
independent GDI capture of the same screen (sparse 32x18 samples, per-channel
tolerance 32) and records the agreeing fraction as `capture_agreement`, so an
empty frame cannot pass silently; it reports 1.0 on this machine and 0.11 when
the frame skip is disabled. The harness verifies an exact cropped source pixel,
identical
samples within an image-anchored mosaic block, annotation rendering, and PNG
readback. It then rebuilds every device-dependent resource on a fresh D3D11
device from the retained base image and requires the re-exported pixels to match
the pre-rebuild bytes exactly. A separate GPU test verifies monotonic luminance
and opaque alpha through float scRGB, tone mapping, and sRGB color conversion.

One debug 4K smoke frame measured 3.832 ms CPU submission, 0.285 ms GPU timestamp
duration, and 0.420 ms blocking Present duration. One Release 4K smoke frame,
including the rebuild pass, measured 3.198 ms CPU submission, 0.292 ms GPU
timestamp duration, and 0.478 ms blocking Present duration. These are isolated
frames in a hidden test window, not frame-rate results. The GPU timer excludes
capture and does not measure compositor latency. No 120 Hz claim is made from
these samples.

## 4K Annotation Stress

`snaplight-editor.exe --stress` captures the real 4K desktop, loads 1000
annotations, and renders 240 frames while redrawing the magnifier and a 600-point
live stroke every frame, reporting per-frame percentiles.

| Metric | p50 | p95 | p99 | max |
| --- | ---: | ---: | ---: | ---: |
| CPU submission | 0.902 ms | 1.482 ms | 1.874 ms | 3.486 ms |
| Blocking Present | 15.750 ms | 16.401 ms | 16.500 ms | 16.798 ms |
| D3D11 GPU timestamp | 16.230 ms | 16.789 ms | 16.996 ms | 17.305 ms |

A 640x400 control frame reports almost identical Present and GPU timestamps
(15.877 ms and 15.716 ms p50), which shows both are gated by the 60 Hz vertical
blank rather than by workload: the blocking Present waits for vsync and the GPU
timestamp completes only when the queue drains at the next vblank. CPU submission
is therefore the only metric here that reflects workload, and every sample stays
well under the 8.33 ms 120 Hz budget. Real GPU cost, displayed-frame intervals
and input-to-present latency remain unmeasured and require a 120/144 Hz display.

## Long Run and Idle

100 consecutive editor sessions (start, 4K capture, render, export, exit) on the
real desktop completed with 0 failures and no leftover processes: first run
370.6 ms, median 369.5 ms, p95 410.8 ms, max 439.7 ms, fitted slope +0.045 ms per
cycle, and no drift between the first and last quarter of the run. Cold 4K
Desktop Duplication capture measured about 143 ms. The per-session figure also
includes process start and the Slint toolbar snapshot, so it is not a pure
capture latency.

Two 60 s windows each of idle core and of five 1920x1080 layered pins showed no
memory growth: idle core stayed at 0.957 MiB private working set and 1.359 MiB
private commit with 0 ms CPU; the pin set moved -0.012 MiB private working set and
-0.023 MiB private commit with unchanged shared commit. Pin CPU is event-driven,
not periodic: the first 10 s window after creation spends about 1.9 s, while five
consecutive following 10 s windows each measured exactly 0 ms.

## Editor Session Memory

A capture session keeps the immutable base image in CPU memory (BGRA8, `stride`
bytes per row) so a lost device can be rebuilt without re-capturing or discarding
the document; a 3840x2160 base is about 31.6 MiB. The editor process is transient
and releases this with the session, so it is not part of the resident-process
numbers above. The CPU copy is created once per session from the normalized SDR
surface; interactive drawing, mosaics, the magnifier, and export stay on the GPU.

## Reproduce

```powershell
.\scripts\dev.ps1 build -Release
.\scripts\dev.ps1 smoke -Release
.\scripts\resource-scenarios.ps1 -Profile release -Seconds 10
.\scripts\dev.ps1 clippy
# 4K/1000-annotation frame-timing stress (writes .local telemetry JSON)
& .\target\release\snaplight-editor.exe --stress
```

Artifacts are written under `.local/smoke` and `.local/resource-scenarios`.
Before 120 Hz acceptance, use a 120/144 Hz monitor, record displayed-frame
intervals and input-to-present latency, and exercise 4K selection dragging,
magnifier movement, mosaic edits, long pen strokes, and at least 1000 annotations.
Measure p50/p95/p99 frame timing and missed frames; 120 Hz has an 8.33 ms budget.
Also exercise mixed-DPI/rotated multi-monitor layouts, capture-access loss,
locked/unlocked sessions, hotplug, and cross-process click-through with real
input. In-session device reconstruction from the retained base image is
implemented and exercised by the smoke rebuild pass; a real device-removal
event, locked/unlocked sessions, and capture-access loss have not yet been
triggered on hardware, so those paths remain unverified. Display configuration
changes now rebuild resources and keep the session instead of ending it.
