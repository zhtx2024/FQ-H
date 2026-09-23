# 版本号同步脚本:单一来源为根 Cargo.toml 的 [workspace.package].version
#
# 用法:
#   .\scripts\bump-version.ps1 -Bump patch     # 修复类改动   0.2.0 → 0.2.1
#   .\scripts\bump-version.ps1 -Bump minor     # 新增功能     0.2.0 → 0.3.0
#   .\scripts\bump-version.ps1 -Bump major     # 破坏性变更   0.2.0 → 1.0.0
#   .\scripts\bump-version.ps1 -Version 1.2.3  # 直接指定
#
# 同步的三处(缺少任何一处都会导致安装包/关于页版本不一致):
#   1) Cargo.toml                     → Rust 侧 env!("CARGO_PKG_VERSION")
#   2) apps/desktop/frontend/package.json → Vite define __APP_VERSION__(联系人列表 NEW 标签用)
#   3) apps/desktop/tauri.conf.json   → 安装包文件名与 Windows 资源版本

param(
    [Parameter(Mandatory = $false)][string]$Version,
    [ValidateSet('major', 'minor', 'patch')][string]$Bump = 'patch'
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$cargoToml = Join-Path $root 'Cargo.toml'
$pkgJson = Join-Path $root 'apps/desktop/frontend/package.json'
$tauriConf = Join-Path $root 'apps/desktop/tauri.conf.json'

$utf8 = New-Object System.Text.UTF8Encoding($false)
$semverPattern = '(?m)^version\s*=\s*"(\d+)\.(\d+)\.(\d+)"'
$jsonPattern = '(?m)^(\s*)"version":\s*"\d+\.\d+\.\d+"'

# ── 读取当前版本(单一来源:Cargo.toml)──
$cargoText = [System.IO.File]::ReadAllText($cargoToml, [System.Text.Encoding]::UTF8)
$match = [regex]::Match($cargoText, $semverPattern)
if (-not $match.Success) { throw "无法在 $cargoToml 中找到版本号" }
$current = "$($match.Groups[1].Value).$($match.Groups[2].Value).$($match.Groups[3].Value)"

# ── 计算目标版本 ──
if (-not $Version) {
    $major = [int]$match.Groups[1].Value
    $minor = [int]$match.Groups[2].Value
    $patch = [int]$match.Groups[3].Value
    switch ($Bump) {
        'major' { $major++; $minor = 0; $patch = 0 }
        'minor' { $minor++; $patch = 0 }
        'patch' { $patch++ }
    }
    $Version = "$major.$minor.$patch"
}
if ($Version -notmatch '^\d+\.\d+\.\d+$') {
    throw "版本号必须形如 x.y.z,收到: $Version"
}

# ── 写回三处 ──
$updated = @()

$newCargo = [regex]::Replace($cargoText, $semverPattern, "version = `"$Version`"", 1)
if ($newCargo -ne $cargoText) {
    [System.IO.File]::WriteAllText($cargoToml, $newCargo, $utf8)
    $updated += 'Cargo.toml'
}

foreach ($file in @($pkgJson, $tauriConf)) {
    $text = [System.IO.File]::ReadAllText($file, [System.Text.Encoding]::UTF8)
    $newText = [regex]::Replace($text, $jsonPattern, "`$1`"version`": `"$Version`"", 1)
    if ($newText -ne $text) {
        [System.IO.File]::WriteAllText($file, $newText, $utf8)
        $updated += (Split-Path -Leaf (Split-Path -Parent $file)) + '/' + (Split-Path -Leaf $file)
    }
}

Write-Output "版本号: $current -> $Version (bump=$Bump)"
Write-Output "已更新: $($updated -join ', ')"
Write-Output ""
Write-Output "提示:重新构建后失效范围"
Write-Output "  - 安装包名变为 feiqiu-r_${Version}_x64-setup.exe"
Write-Output "  - 关于页/联系人 NEW 标签读到的都是 $Version"
