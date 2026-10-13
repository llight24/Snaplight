# 架构防回退检查（只读）。
# 默认在仓库根运行 `cargo metadata --format-version 1 --no-deps --locked` 并检查退出码；
# 也可用 -MetadataPath 读取同 schema 的本地 JSON 样本，避免依赖已安装的 cargo。
#
# 判定规则：
#  - 以 workspace_members 构建实际内部包图。
#  - 普通依赖与 build 依赖按 package 原始名字（dependency.name）识别；依赖 rename 仍按原包名判定。
#  - dev-dependencies 不属于产品依赖，不参与边界判定（plugin-host 测试合法依赖 render-win 即被忽略）。
#  - target 条件（cfg(...)）不忽略：target 限定的依赖同样计入边界。
#  - 未登记的 workspace 包、超出允许矩阵的内部依赖（含反向依赖）、domain 直接依赖原生/UI/GPU crate 全部失败。
#  - 失败时打印全部问题并 exit 1；成功时报告包数与结论。
[CmdletBinding()]
param(
    # 可选：读取同 schema 的本地 metadata JSON（不调用 cargo）。
    [string]$MetadataPath
)

$ErrorActionPreference = 'Stop'

# 允许矩阵：包名 -> 允许的内部产品依赖（普通 + build）。空数组表示不允许任何内部依赖。
$Allowed = [ordered]@{
    'snaplight-domain'         = @()
    'snaplight-platform'       = @('snaplight-domain')
    'snaplight-render-win'     = @('snaplight-domain', 'snaplight-platform')
    'snaplight-ui'             = @('snaplight-domain')
    'snaplight-core'           = @('snaplight-domain', 'snaplight-platform', 'snaplight-plugin-api')
    'snaplight-editor'         = @('snaplight-domain', 'snaplight-platform', 'snaplight-render-win', 'snaplight-ui')
    'snaplight-pins'           = @('snaplight-domain', 'snaplight-platform', 'snaplight-render-win')
    'snaplight-plugin-api'     = @('snaplight-domain')
    'snaplight-plugin-host'    = @('snaplight-domain', 'snaplight-platform', 'snaplight-plugin-api')
    'snaplight-example-plugin' = @('snaplight-domain', 'snaplight-plugin-api')
}

$DomainName = 'snaplight-domain'
# domain 必须与平台/UI/GPU 相关 crate 隔离（含 build 依赖）。
$DomainForbidden = @('windows', 'windows-core', 'slint', 'slint-build', 'wgpu')

$Root = Split-Path -Parent $PSScriptRoot

function Get-MetadataJson {
    param([string]$Path)
    if ($Path) {
        if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
            throw "指定的 -MetadataPath 不存在：$Path"
        }
        return Get-Content -LiteralPath $Path -Raw
    }
    Push-Location -LiteralPath $Root
    try {
        $raw = (& cargo metadata --format-version 1 --no-deps --locked) -join "`n"
        $code = $LASTEXITCODE
    }
    finally {
        Pop-Location
    }
    if ($code -ne 0) {
        throw "cargo metadata 退出码为 $code。"
    }
    return $raw
}

try {
    $json = Get-MetadataJson -Path $MetadataPath
    $meta = $json | ConvertFrom-Json
}
catch {
    Write-Host "架构检查失败：$($_.Exception.Message)" -ForegroundColor Red
    exit 1
}

$violations = New-Object System.Collections.Generic.List[string]

$pkgById = @{}
foreach ($p in @($meta.packages)) { $pkgById[[string]$p.id] = $p }

$memberIds = @($meta.workspace_members)
if ($memberIds.Count -eq 0) {
    Write-Host "架构检查失败：cargo metadata 未返回 workspace_members。" -ForegroundColor Red
    exit 1
}

# 内部包表：原包名 -> package 对象（仅 workspace 成员）。
$internal = @{}
foreach ($id in $memberIds) {
    $key = [string]$id
    if (-not $pkgById.ContainsKey($key)) {
        $violations.Add("workspace 成员 $key 未出现在 metadata.packages 中。")
        continue
    }
    $internal[[string]$pkgById[$key].name] = $pkgById[$key]
}

# 1) 未登记的 workspace 包直接失败。
foreach ($name in $internal.Keys) {
    if (-not $Allowed.Contains($name)) {
        $violations.Add("workspace 包 '$name' 未在允许矩阵中登记。")
    }
}

# 2) 内部依赖边界（普通 + build，忽略 dev；target 限定不计忽略）。
foreach ($name in $internal.Keys) {
    $pkg = $internal[$name]
    foreach ($dep in @($pkg.dependencies)) {
        if ([string]$dep.kind -eq 'dev') { continue }
        $depName = [string]$dep.name
        if (-not $internal.ContainsKey($depName)) { continue }
        if ($name -eq $DomainName) {
            $violations.Add("$DomainName 依赖内部包 '$depName'，domain 不允许任何内部依赖（禁止反向依赖）。")
            continue
        }
        if (-not $Allowed.Contains($name)) { continue }
        if ($Allowed[$name] -notcontains $depName) {
            $violations.Add("$name 依赖 '$depName' 超出允许矩阵。")
        }
    }
}

# 3) domain 禁止直接依赖原生/UI/GPU crate（含 build 依赖）。
if ($internal.ContainsKey($DomainName)) {
    foreach ($dep in @($internal[$DomainName].dependencies)) {
        $depName = [string]$dep.name
        if ($DomainForbidden -contains $depName) {
            $violations.Add("$DomainName 直接依赖禁用的平台/UI/GPU crate '$depName'。")
        }
    }
}

if ($violations.Count -gt 0) {
    Write-Host "架构检查失败（$($violations.Count) 项）：" -ForegroundColor Red
    foreach ($v in $violations) { Write-Host "  - $v" }
    exit 1
}

Write-Host ("架构检查通过：{0} 个 workspace 包，内部依赖边界符合允许矩阵。" -f $internal.Count)
exit 0
