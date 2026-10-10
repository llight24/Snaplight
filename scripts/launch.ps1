<#
.SYNOPSIS
    一键启动 Snaplight 做手动测试（或一键停止）。

.DESCRIPTION
    默认先做 Release 构建（可用 -SkipBuild 跳过），再启动常驻托盘进程
    snaplight-core.exe。该进程持有 Ctrl+Shift+A 截图快捷键和 Ctrl+Shift+P
    贴图恢复快捷键，并按需拉起编辑器、贴图管理和插件宿主。启动后打印手动
    测试清单，脚本立即返回，软件继续在后台运行。

.PARAMETER SkipBuild
    跳过构建，直接用 target\<profile>\ 下已有的可执行文件。

.PARAMETER Debug
    使用 debug 构建而不是 release。

.PARAMETER Settings
    不启动托盘，直接打开编辑器的设置窗口。

.PARAMETER Stop
    优雅退出正在运行的 Snaplight 进程（先给托盘发 WM_CLOSE，再清理残留）。

.PARAMETER Restart
    先停掉正在运行的实例，再重新构建并启动。用于「改完代码马上看效果」，因为
    Windows 不允许替换正在运行的 exe，热重载不存在，必须重启进程。

.EXAMPLE
    .\scripts\launch.ps1
    .\scripts\launch.ps1 -SkipBuild
    .\scripts\launch.ps1 -Restart
    .\scripts\launch.ps1 -Settings
    .\scripts\launch.ps1 -Stop
#>
param(
    [switch]$SkipBuild,
    [switch]$Debug,
    [switch]$Settings,
    [switch]$Stop,
    [switch]$Restart
)
$ErrorActionPreference='Stop'
$workspace=Split-Path -Parent $PSScriptRoot
$profile=if ($Debug) {'debug'} else {'release'}
$binaryDirectory=Join-Path $workspace "target\$profile"

if (-not ('SnaplightLifecycle' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class SnaplightLifecycle {
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindow(string cls, string title);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr window, out uint pid);
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr window, uint message, IntPtr w, IntPtr l);
}
'@
}
# Only touch processes started from this build directory, never another checkout.
function Get-SnaplightProcess([string]$Name) {
    @(Get-Process -Name $Name -ErrorAction SilentlyContinue | Where-Object {
        $_.Path -and ([IO.Path]::GetDirectoryName($_.Path) -eq $binaryDirectory)
    })
}
function Stop-Snaplight {
    foreach ($process in Get-SnaplightProcess 'snaplight-core') {
        $window=[SnaplightLifecycle]::FindWindow('Snaplight.Core',$null)
        [uint32]$owner=0
        if ($window -ne [IntPtr]::Zero) {
            [void][SnaplightLifecycle]::GetWindowThreadProcessId($window,[ref]$owner)
        }
        if ($owner -eq $process.Id) {
            # Tray "exit" path: WM_CLOSE, so the core runs its own cleanup.
            [void][SnaplightLifecycle]::PostMessage($window,0x10,[IntPtr]::Zero,[IntPtr]::Zero)
        }
    }
    Start-Sleep -Milliseconds 800
    foreach ($name in @('snaplight-core','snaplight-editor','snaplight-pins','snaplight-plugin-host')) {
        foreach ($process in Get-SnaplightProcess $name) {
            if (!$process.WaitForExit(3000)) { $process.Kill(); $process.WaitForExit() }
        }
    }
}

if ($Stop) {
    Stop-Snaplight
    Write-Output 'Snaplight stopped.'
    exit 0
}

if ($Restart) {
    # Windows cannot replace a running executable, so the old process has to go
    # before the build; otherwise cargo fails with "拒绝访问 (os error 5)".
    $existing=@(Get-SnaplightProcess 'snaplight-core')
    if ($existing.Count -gt 0) {
        Write-Output "Restarting: stopping snaplight-core (pid $($existing[0].Id)) and its children..."
    }
    Stop-Snaplight
}

if (!$SkipBuild) {
    Write-Output "Building ($profile)..."
    # Named splat, not an array: array splatting would pass "-Release" as a
    # positional value instead of binding the switch, silently building debug.
    $devArguments=@{ Action='build' }
    if (!$Debug) { $devArguments['Release']=$true }
    & (Join-Path $PSScriptRoot 'dev.ps1') @devArguments
    if ($LASTEXITCODE -ne 0) { throw "Build failed with exit code $LASTEXITCODE." }
    $built=Join-Path $binaryDirectory 'snaplight-core.exe'
    if (!(Test-Path -LiteralPath $built)) { throw "Build reported success but $built is missing." }
}

if ($Settings) {
    $editor=Join-Path $binaryDirectory 'snaplight-editor.exe'
    $process=Start-Process -FilePath $editor -ArgumentList '--settings' -PassThru
    Start-Sleep -Milliseconds 1200
    $process.Refresh()
    if ($process.HasExited) { throw "snaplight-editor --settings exited immediately (code $($process.ExitCode))." }
    Write-Output "Settings window open (pid $($process.Id))."
    exit 0
}

$running=@(Get-SnaplightProcess 'snaplight-core')
if ($running.Count -gt 0) {
    Write-Output "snaplight-core is already running (pid $($running[0].Id))."
    Write-Output 'Stop it first with: .\scripts\launch.ps1 -Stop'
    exit 1
}

$core=Start-Process -FilePath (Join-Path $binaryDirectory 'snaplight-core.exe') -PassThru
Start-Sleep -Milliseconds 1500
$core.Refresh()
if ($core.HasExited) {
    throw "snaplight-core exited immediately (code $($core.ExitCode)). Check the tray/hotkey and any error dialog."
}
Write-Output @"
Snaplight is running (pid $($core.Id)). Look for the tray icon, then test:

  1. Ctrl+Shift+A          start a capture; drag to select, Esc cancels.
  2. Toolbar               rectangle / ellipse / arrow / pen / mosaic / text,
                           width, mosaic block, colour, live text input.
  3. Ctrl+Z / Ctrl+Y       undo / redo annotations.
  4. Ctrl+C                copy the selection to the clipboard.
  5. Ctrl+S                save source + document + PNG under the capture folder.
  6. Enter                 flatten and pin the selection to the screen.
  7. Pin window            drag to move, wheel to scale, Ctrl+wheel opacity,
                           right click for the menu, "Click through" makes it
                           transparent to input; Ctrl+Shift+P restores it.
  8. Settings               .\scripts\launch.ps1 -Settings
  9. Resource scenarios     .\scripts\resource-scenarios.ps1 -Profile $profile -Seconds 10
 10. Frame-timing stress    .\target\$profile\snaplight-editor.exe --stress

Stop everything with the tray menu, or:
  .\scripts\launch.ps1 -Stop
"@
