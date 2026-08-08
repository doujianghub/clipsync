<#
.SYNOPSIS
    把 ClipSync 打包成 Windows 发布压缩包。

.DESCRIPTION
    产出 dist\ClipSync-<版本>-windows-x64.zip，内含可执行文件、许可证与说明。

    与 macOS 那边不同，Windows 不需要 .app 那样的目录结构，一个 exe 就够——
    所以这里只做构建、自检、打包三件事。

    **必须是 release 构建**：GUI 子系统声明写的是
    `cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")`，
    debug 下会带一个控制台黑窗，托盘程序不该有那个东西。

.PARAMETER Version
    版本号（不带 v 前缀）。省略时从 Cargo.toml 读取。

.EXAMPLE
    scripts\package-windows.ps1
    scripts\package-windows.ps1 -Version 1.0.0
#>

[CmdletBinding()]
param(
    [string]$Version
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    if (-not $Version) {
        $line = Select-String -Path 'Cargo.toml' -Pattern '^version\s*=\s*"([^"]+)"' |
                Select-Object -First 1
        if (-not $line) { throw '无法从 Cargo.toml 读出版本号，请用 -Version 指定' }
        $Version = $line.Matches[0].Groups[1].Value
    }
    Write-Host "打包 ClipSync $Version (windows-x64)" -ForegroundColor Cyan

    Write-Host '构建 release...' -ForegroundColor Cyan
    cargo build --release --locked
    if ($LASTEXITCODE -ne 0) { throw 'cargo build 失败' }

    $exe = Join-Path $root 'target\release\clipsync.exe'
    if (-not (Test-Path $exe)) { throw "构建产物不存在: $exe" }

    # 自检：确认是 GUI 子系统。
    #
    # 这一项值得单独验，因为它出错的表现很隐蔽——程序照常能用，只是每次开机
    # 自启都会闪出一个黑色控制台窗口，而打包的人在命令行里跑根本看不出来。
    $bytes = [System.IO.File]::ReadAllBytes($exe)
    $peOffset = [BitConverter]::ToInt32($bytes, 0x3C)
    $subsystem = [BitConverter]::ToUInt16($bytes, $peOffset + 0x5C)
    # 2 = IMAGE_SUBSYSTEM_WINDOWS_GUI, 3 = WINDOWS_CUI
    if ($subsystem -ne 2) {
        throw "可执行文件不是 GUI 子系统（subsystem=$subsystem），开机自启会闪控制台窗口"
    }
    Write-Host '  ✓ GUI 子系统' -ForegroundColor Green

    $size = [math]::Round((Get-Item $exe).Length / 1MB, 2)
    Write-Host "  ✓ 二进制 $size MB" -ForegroundColor Green

    # 组装分发目录。
    $stage = Join-Path $root "target\pkg-windows\ClipSync-$Version"
    if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
    New-Item -ItemType Directory -Path $stage -Force | Out-Null

    Copy-Item $exe $stage
    foreach ($f in 'README.md', 'README.zh-CN.md', 'CHANGELOG.md',
                   'LICENSE-MIT', 'LICENSE-APACHE') {
        if (Test-Path (Join-Path $root $f)) { Copy-Item (Join-Path $root $f) $stage }
    }

    $dist = Join-Path $root 'dist'
    New-Item -ItemType Directory -Path $dist -Force | Out-Null
    $zip = Join-Path $dist "ClipSync-$Version-windows-x64.zip"
    if (Test-Path $zip) { Remove-Item $zip -Force }

    Compress-Archive -Path "$stage\*" -DestinationPath $zip -CompressionLevel Optimal

    $zipSize = [math]::Round((Get-Item $zip).Length / 1KB, 0)
    Write-Host ''
    Write-Host "完成: $zip ($zipSize KB)" -ForegroundColor Green
    Write-Host ''
    Write-Host '给别人用时请一并转告：SmartScreen 首次运行可能拦截'
    Write-Host '（没有代码签名证书的固有限制），选「更多信息 → 仍要运行」。'
}
finally {
    Pop-Location
}
