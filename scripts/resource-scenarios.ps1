param([ValidateSet('debug','release')][string]$Profile='release',[ValidateRange(1,30)][int]$Seconds=10)
$ErrorActionPreference='Stop'
$workspace=Split-Path -Parent $PSScriptRoot
$directory=Join-Path $workspace '.local\resource-scenarios'
New-Item -ItemType Directory -Force -Path $directory | Out-Null
$env:SNAPLIGHT_DATA_DIR=$directory
$env:SNAPLIGHT_NONINTERACTIVE='1'
if (Get-Process -Name 'snaplight-core','snaplight-pins' -ErrorAction SilentlyContinue) {throw 'Close existing Snaplight instances before measuring isolated scenarios'}
$core=Start-Process -FilePath (Join-Path $workspace "target\$Profile\snaplight-core.exe") -PassThru -WindowStyle Hidden -RedirectStandardError (Join-Path $directory 'core.err.log')
$pinProcess=$null
Add-Type -TypeDefinition @'
public static class SnaplightFixture {
    public static byte[] Pixels(int length) {
        var pixels=new byte[length];
        for (int i=0;i<length;i+=4) {pixels[i]=80;pixels[i+1]=180;pixels[i+2]=40;pixels[i+3]=255;}
        return pixels;
    }
}
'@
try {
    Start-Sleep -Seconds 1
    if ($core.HasExited) {throw 'Core failed to start'}
    & "$PSScriptRoot\measure-resources.ps1" -Profile $Profile -Seconds $Seconds -Output '.local\resource-scenarios\idle.json'
    $start=New-Object Diagnostics.ProcessStartInfo
    $start.FileName=Join-Path $workspace "target\$Profile\snaplight-pins.exe"
    $start.UseShellExecute=$false;$start.CreateNoWindow=$true;$start.RedirectStandardInput=$true;$start.RedirectStandardError=$true
    $pinProcess=[Diagnostics.Process]::Start($start)
    foreach ($scenario in @(@{name='five-1080p';width=1920;height=1080;count=5},@{name='two-4k';width=3840;height=2160;count=2},@{name='twenty-1080p';width=1920;height=1080;count=20})) {
        $image=[ordered]@{id=1;width=$scenario.width;height=$scenario.height;stride=$scenario.width*4;format='Bgra8';alpha='Opaque';color_space='Srgb'}
        $path=Join-Path $directory "$($scenario.name).bgra"
        $header=[Text.Encoding]::UTF8.GetBytes(($image | ConvertTo-Json -Compress))
        $pixels=[SnaplightFixture]::Pixels($image.stride*$image.height)
        $file=[IO.File]::Create($path);$writer=[IO.BinaryWriter]::new($file)
        try {$writer.Write([Text.Encoding]::ASCII.GetBytes('SNAPBG01'));$writer.Write([uint32]$header.Length);$writer.Write($header);$writer.Write($pixels)} finally {$writer.Dispose()}
        $pixels=$null
        $doc=Join-Path $directory "$($scenario.name).json"
        [IO.File]::WriteAllText($doc,([ordered]@{version=1;image=$image;selection=$null;annotations=@()} | ConvertTo-Json -Depth 5 -Compress))
        for ($index=0;$index -lt $scenario.count;$index++) {
            $message=@{version=1;payload=@{Open=@{image=$path;document=$doc}}} | ConvertTo-Json -Depth 5 -Compress
            $pinProcess.StandardInput.WriteLine($message);$pinProcess.StandardInput.Flush()
        }
        Start-Sleep -Seconds 2
        if ($pinProcess.HasExited) {throw "Pin manager failed: $($pinProcess.StandardError.ReadToEnd())"}
        & "$PSScriptRoot\measure-resources.ps1" -Profile $Profile -Seconds $Seconds -Output ".local\resource-scenarios\$($scenario.name)-resources.json"
        # Closing all pins exits the GPU process, so start a new host for the next scenario.
        $pinProcess.StandardInput.WriteLine('{"version":1,"payload":"Shutdown"}');$pinProcess.StandardInput.Flush()
        if (!$pinProcess.WaitForExit(10000)) {$pinProcess.Kill();$pinProcess.WaitForExit();throw 'Pin manager shutdown timed out'}
        if ($pinProcess.ExitCode -ne 0) {throw "Pin manager failed: $($pinProcess.StandardError.ReadToEnd())"}
        if ($scenario.name -ne 'twenty-1080p') {$pinProcess=[Diagnostics.Process]::Start($start)}
    }
} finally {
    if ($pinProcess -and !$pinProcess.HasExited) {$pinProcess.Kill();$pinProcess.WaitForExit()}
    if (!$core.HasExited) {
        Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class SnaplightShutdown {
    [DllImport("user32.dll",CharSet=CharSet.Unicode)] public static extern IntPtr FindWindow(string cls,string title);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr window,out uint pid);
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr window,uint message,IntPtr w,IntPtr l);
}
'@
        $window=[SnaplightShutdown]::FindWindow('Snaplight.Core',$null);[uint32]$owner=0
        [void][SnaplightShutdown]::GetWindowThreadProcessId($window,[ref]$owner)
        if ($owner -eq $core.Id) {[void][SnaplightShutdown]::PostMessage($window,0x10,[IntPtr]::Zero,[IntPtr]::Zero)}
        if (!$core.WaitForExit(5000)) {$core.Kill();$core.WaitForExit()}
    }
}
