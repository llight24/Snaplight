# Stop the running Snaplight, build Release, and start the tray process again.
# No tests. Equivalent to .\scripts\launch.ps1 -Restart.
$ErrorActionPreference = 'Stop'
& (Join-Path $PSScriptRoot 'launch.ps1') -Restart
