<#
.SYNOPSIS
  构建 wx-tool 的 MSI 安装包（仅当前用户安装）。

.DESCRIPTION
  流程：cargo build --release  →  wix build  →  target\wix\wx-tool-<版本>-x64.msi

  依赖 WiX Toolset 6/7（提供 wix.exe）。装法：
      scoop install wixtoolset
  或  dotnet tool install --global wix     （需要 .NET SDK）

  WiX 自 6.0 起要求显式接受 OSMF EULA。本脚本传 --acceptEula，
  即运行脚本即代表你接受该协议。协议要点：年收入低于 10000 美元的项目免费使用。
  详情见 https://wixtoolset.org/legal/

.PARAMETER Configuration
  cargo 的构建配置，默认 release。

.PARAMETER SkipBuild
  跳过 cargo build，直接拿现有的二进制打包。

.EXAMPLE
  pwsh packaging\build-msi.ps1
#>
[CmdletBinding()]
param(
    [ValidateSet('release', 'debug')]
    [string]$Configuration = 'release',
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$ProjectDir = Split-Path -Parent $PSScriptRoot
Push-Location $ProjectDir
try {
    # ---- 前置检查 ----
    $wix = Get-Command wix -ErrorAction SilentlyContinue
    if (-not $wix) {
        throw @"
找不到 wix.exe。请先安装 WiX Toolset 6 或更高版本：
    scoop install wixtoolset
或（需要 .NET SDK）：
    dotnet tool install --global wix
"@
    }
    $wixVersion = (& wix --version) -join ''
    Write-Host "WiX: $wixVersion" -ForegroundColor DarkGray

    # WiX 6+ 要求显式接受 OSMF EULA，eulaId 随大版本走（7.x -> "wix7"）。
    # 从版本号推导，这样升到 WiX 8 也不用改脚本。
    if ($wixVersion -notmatch '^(\d+)\.') { throw "无法解析 WiX 版本号：$wixVersion" }
    $eulaId = "wix$($Matches[1])"

    # ---- 版本号从 Cargo.toml 读，避免两处各写一遍 ----
    $cargoToml = Get-Content (Join-Path $ProjectDir 'Cargo.toml') -Raw
    if ($cargoToml -notmatch '(?m)^\s*version\s*=\s*"([^"]+)"') {
        throw '无法从 Cargo.toml 解析出 version'
    }
    $version = $Matches[1]

    # MSI 的 ProductVersion 只认 主.次.修订 三段数字，把 -alpha 之类的后缀去掉
    $msiVersion = ($version -split '[-+]')[0]
    if ($msiVersion -notmatch '^\d+(\.\d+){0,3}$') {
        throw "版本号 '$version' 无法转换成 MSI 可用的形式"
    }
    Write-Host "版本: $version (MSI ProductVersion = $msiVersion)" -ForegroundColor DarkGray

    # ---- 构建 ----
    if (-not $SkipBuild) {
        Write-Host "`n[1/2] cargo build --$Configuration" -ForegroundColor Cyan
        if ($Configuration -eq 'release') {
            & cargo build --release
        } else {
            & cargo build
        }
        if ($LASTEXITCODE -ne 0) { throw "cargo build 失败（退出码 $LASTEXITCODE）" }
    } else {
        Write-Host "`n[1/2] 跳过 cargo build" -ForegroundColor DarkGray
    }

    $binDir = Join-Path $ProjectDir "target\$Configuration"
    $exe = Join-Path $binDir 'wx-tool.exe'
    if (-not (Test-Path $exe)) { throw "找不到 $exe，请先构建" }

    # ---- 打包 ----
    $outDir = Join-Path $ProjectDir 'target\wix'
    New-Item -ItemType Directory -Force $outDir | Out-Null
    $msi = Join-Path $outDir "wx-tool-$version-x64.msi"

    Write-Host "[2/2] wix build -> $msi" -ForegroundColor Cyan
    # --acceptEula / --nologo 是全局选项，必须放在 build 子命令**之前**，
    # 且 --acceptEula 需要跟上 eulaId（不是单纯的开关）
    & wix --acceptEula $eulaId build `
        (Join-Path $PSScriptRoot 'wx-tool.wxs') `
        -arch x64 `
        -define "Version=$msiVersion" `
        -define "BinDir=$binDir" `
        -define "ProjectDir=$ProjectDir" `
        -out $msi
    if ($LASTEXITCODE -ne 0) { throw "wix build 失败（退出码 $LASTEXITCODE）" }

    $size = (Get-Item $msi).Length
    Write-Host "`n完成: $msi" -ForegroundColor Green
    Write-Host ("大小: {0:N1} MB" -f ($size / 1MB)) -ForegroundColor Green
    Write-Host @"

安装（仅当前用户，不需要管理员）:
    msiexec /i "$msi"
静默安装:
    msiexec /i "$msi" /qn
卸载:
    msiexec /x "$msi"
"@ -ForegroundColor DarkGray
}
finally {
    Pop-Location
}
