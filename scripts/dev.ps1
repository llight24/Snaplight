param([ValidateSet('build','check','test','run','smoke','plugin','fmt','clippy')][string]$Action='build',[switch]$Release)
$ErrorActionPreference='Stop'
$workspace=Split-Path -Parent $PSScriptRoot
Set-Location -LiteralPath $workspace
$localCargo=Join-Path $workspace '.tools\cargo\bin'
if (Test-Path -LiteralPath $localCargo) {
    $env:RUSTUP_HOME=Join-Path $workspace '.tools\rustup'
    $env:CARGO_HOME=Join-Path $workspace '.tools\cargo'
    $env:PATH="$localCargo;$env:PATH"
    $bundledLinker=Join-Path $env:RUSTUP_HOME 'toolchains\stable-x86_64-pc-windows-gnu\lib\rustlib\x86_64-pc-windows-gnu\bin\rust-lld.exe'
    if (Test-Path -LiteralPath $bundledLinker) { $env:CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=$bundledLinker; $env:RUSTFLAGS='-C linker-flavor=ld.lld -C link-self-contained=yes' }
    $llvm=Get-ChildItem -LiteralPath (Join-Path $workspace '.tools') -Directory -Filter 'llvm-mingw-*-ucrt-x86_64' | Sort-Object Name -Descending | Select-Object -First 1
    if ($llvm) {
        $env:PATH="$($llvm.FullName)\bin;$env:PATH"
        $imports=Join-Path $workspace '.tools\win-imports'
        New-Item -ItemType Directory -Force -Path $imports | Out-Null
        Copy-Item -LiteralPath (Join-Path $llvm.FullName 'x86_64-w64-mingw32\lib\libshlwapi.a') -Destination $imports
        $env:SNAPLIGHT_MINGW_LIB=$imports
    }
}
$profile=if ($Release) {'release'} else {'debug'}
$buildArgs=@('build','--workspace','--locked')
if ($Release) {$buildArgs+='--release'}
switch ($Action) {
    'build' { & cargo @buildArgs }
    'check' { & cargo check --workspace --locked }
    'test' { & cargo build --workspace --locked; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; & cargo test --workspace --locked }
    'fmt' { & cargo fmt --all -- --check }
    'clippy' { & cargo clippy --workspace --all-targets --locked -- -D warnings }
    'run' { & (Join-Path $workspace "target\$profile\snaplight-core.exe") }
    'plugin' { & (Join-Path $workspace "target\$profile\snaplight-core.exe") --plugin-check }
    'smoke' {
        $env:SNAPLIGHT_DATA_DIR=Join-Path $workspace '.local\smoke'
        $env:SNAPLIGHT_NONINTERACTIVE='1'
        New-Item -ItemType Directory -Force -Path $env:SNAPLIGHT_DATA_DIR | Out-Null
        foreach ($app in @('snaplight-core','snaplight-editor','snaplight-pins')) {
            $errorFile=Join-Path $env:SNAPLIGHT_DATA_DIR "$app.err.log"
            $process=Start-Process -FilePath (Join-Path $workspace "target\$profile\$app.exe") -ArgumentList '--smoke' -PassThru -WindowStyle Hidden -RedirectStandardError $errorFile
            if (!$process.WaitForExit(30000)) { $process.Kill(); $process.WaitForExit(); throw "$app smoke timed out" }
            if ($process.ExitCode -ne 0) { throw "$app smoke failed: $($process.ExitCode). $(Get-Content -LiteralPath $errorFile -Raw)" }
        }
        $errorFile=Join-Path $env:SNAPLIGHT_DATA_DIR 'pins-low-memory.err.log'
        $process=Start-Process -FilePath (Join-Path $workspace "target\$profile\snaplight-pins.exe") -ArgumentList '--smoke-low-memory' -PassThru -WindowStyle Hidden -RedirectStandardError $errorFile
        if (!$process.WaitForExit(30000)) {$process.Kill();$process.WaitForExit();throw 'Low-memory pin smoke timed out'}
        if ($process.ExitCode -ne 0) {throw "Low-memory pin smoke failed: $(Get-Content -LiteralPath $errorFile -Raw)"}
        $process=Start-Process -FilePath (Join-Path $workspace "target\$profile\snaplight-core.exe") -ArgumentList '--plugin-check' -PassThru -WindowStyle Hidden
        if (!$process.WaitForExit(15000)) { $process.Kill(); $process.WaitForExit(); throw 'Plugin smoke timed out' }
        if ($process.ExitCode -ne 0) { throw 'Plugin smoke failed' }
        Write-Output "Smoke artifacts: $env:SNAPLIGHT_DATA_DIR"
    }
}
if ($LASTEXITCODE -ne 0) {exit $LASTEXITCODE}
