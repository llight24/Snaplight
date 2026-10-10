param([ValidateRange(1,60)][int]$Seconds=10,[ValidateSet('debug','release')][string]$Profile='release',[string]$Output='.local\resources.json')
$ErrorActionPreference='Stop'
$workspace=Split-Path -Parent $PSScriptRoot
$binaryDirectory=[IO.Path]::GetFullPath((Join-Path $workspace "target\$Profile"))
if (-not ('SnaplightMemory' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class SnaplightMemory {
    [StructLayout(LayoutKind.Sequential)] public struct Counters {
        public uint Size, Faults;
        public UIntPtr PeakWorkingSet, WorkingSet, PeakPagedPool, PagedPool, PeakNonPagedPool, NonPagedPool, Pagefile, PeakPagefile, PrivateCommit, PrivateWorkingSet, SharedCommit, JobSharedCommit;
    }
    [DllImport("psapi.dll", SetLastError=true)] static extern bool GetProcessMemoryInfo(IntPtr process, out Counters counters, uint size);
    public static Counters Read(IntPtr handle) {
        Counters result;
        if (!GetProcessMemoryInfo(handle, out result, (uint)Marshal.SizeOf(typeof(Counters)))) throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
        return result;
    }
}
'@
}
function Get-SnaplightSample {
    $rows=@()
    foreach ($process in (Get-Process -Name 'snaplight-*' -ErrorAction SilentlyContinue)) {
        try {
            if ([IO.Path]::GetDirectoryName($process.Path) -ne $binaryDirectory) {continue}
            $memory=[SnaplightMemory]::Read($process.Handle)
            $rows += [pscustomobject]@{pid=$process.Id;name=$process.ProcessName;private_working_set=[uint64]$memory.PrivateWorkingSet;private_commit=[uint64]$memory.PrivateCommit;shared_commit=[uint64]$memory.SharedCommit;working_set=[uint64]$memory.WorkingSet;cpu_ms=$process.TotalProcessorTime.TotalMilliseconds}
        } catch {Write-Warning "Unable to sample process $($process.Id): $_"}
    }
    return $rows
}
$start=@(Get-SnaplightSample)
if ($start.Count -eq 0) {throw "No Snaplight processes running from $binaryDirectory"}
$clock=[Diagnostics.Stopwatch]::StartNew()
Start-Sleep -Seconds $Seconds
$end=@(Get-SnaplightSample)
$clock.Stop()
$delta=0.0
foreach ($row in $end) {$previous=$start | Where-Object pid -EQ $row.pid | Select-Object -First 1;if ($previous) {$delta += [Math]::Max(0,$row.cpu_ms-$previous.cpu_ms)}}
$gpu=@();$gpuError=$null
try {
    $sets=Get-Counter -ListSet '*GPU Process Memory*' -ErrorAction Stop
    $paths=@($sets.PathsWithInstances | Where-Object {$_ -match '(Dedicated Usage|Shared Usage)$'})
    if ($paths.Count -eq 0) {throw 'GPU memory counters unavailable or localized'}
    $gpu=@((Get-Counter -Counter $paths -ErrorAction Stop).CounterSamples | Where-Object {$instance=$_.InstanceName; @($end | Where-Object {$instance -match "pid_$($_.pid)_"}).Count -gt 0} | ForEach-Object {[pscustomobject]@{instance=$_.InstanceName;counter=$_.Path;bytes=$_.CookedValue}})
} catch {$gpuError=$_.ToString()}
$report=[ordered]@{
    recorded_at=(Get-Date).ToUniversalTime().ToString('o');sample_seconds=$clock.Elapsed.TotalSeconds;processes=$end;
    total_private_working_set=($end | Measure-Object private_working_set -Sum).Sum;
    total_private_commit=($end | Measure-Object private_commit -Sum).Sum;
    total_shared_commit=($end | Measure-Object shared_commit -Sum).Sum;
    cpu_ms=$delta;cpu_percent_machine=100*$delta/($clock.Elapsed.TotalMilliseconds*[Environment]::ProcessorCount);
    cpu_sample_covers_same_pids=(@($start.pid | Sort-Object) -join ',') -eq (@($end.pid | Sort-Object) -join ',');
    gpu_memory=$gpu;gpu_counter_error=$gpuError;
    computer=(Get-CimInstance Win32_ComputerSystem | Select-Object Manufacturer,Model,TotalPhysicalMemory);
    cpu=(Get-CimInstance Win32_Processor | Select-Object Name,NumberOfLogicalProcessors);
    video=(Get-CimInstance Win32_VideoController | Select-Object Name,DriverVersion,CurrentHorizontalResolution,CurrentVerticalResolution,CurrentRefreshRate)
}
$destination=[IO.Path]::GetFullPath((Join-Path $workspace $Output))
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $destination) | Out-Null
$report | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $destination -Encoding utf8
$report | ConvertTo-Json -Depth 8
