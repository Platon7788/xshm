<#
.SYNOPSIS
    Сборка xshm для всех таргетов с раскладкой артефактов.

.DESCRIPTION
    Собирает библиотеку для MSVC (x64, x86) и MinGW (x64),
    раскладывает результат в:
        target\MSVC\Release\x64\  target\MSVC\Debug\x64\
        target\MSVC\Release\x86\  target\MSVC\Debug\x86\
        target\MINGW\Release\x64\ target\MINGW\Debug\x64\

    Каждая папка содержит .lib/.a и include\xshm.h

.PARAMETER Config
    Release или Debug (по умолчанию Release)

.PARAMETER Target
    all, msvc, mingw, x64, x86 (по умолчанию all)

.EXAMPLE
    .\build.ps1
    .\build.ps1 -Config Debug
    .\build.ps1 -Config Release -Target msvc
    .\build.ps1 -Target x86
#>

param(
    [ValidateSet("Release", "Debug")]
    [string]$Config = "Release",

    [ValidateSet("all", "msvc", "mingw", "x64", "x86")]
    [string]$Target = "all"
)

$ErrorActionPreference = "Stop"

# Таблица таргетов
$targets = @(
    @{ Triple = "x86_64-pc-windows-msvc";  Toolchain = "MSVC";  Arch = "x64"; LibName = "xshm.lib" }
    @{ Triple = "i686-pc-windows-msvc";    Toolchain = "MSVC";  Arch = "x86"; LibName = "xshm.lib" }
    @{ Triple = "x86_64-pc-windows-gnu";   Toolchain = "MINGW"; Arch = "x64"; LibName = "libxshm.a" }
)

# Фильтрация по параметру -Target
$selected = switch ($Target) {
    "msvc"  { $targets | Where-Object { $_.Toolchain -eq "MSVC" } }
    "mingw" { $targets | Where-Object { $_.Toolchain -eq "MINGW" } }
    "x64"   { $targets | Where-Object { $_.Arch -eq "x64" } }
    "x86"   { $targets | Where-Object { $_.Arch -eq "x86" } }
    default { $targets }
}

$cargoDir = if ($Config -eq "Release") { "release" } else { "debug" }
$cargoFlags = if ($Config -eq "Release") { @("--release") } else { @() }

$root = $PSScriptRoot
$failed = @()

Write-Host "=== xshm build ===" -ForegroundColor Cyan
Write-Host "Config: $Config | Targets: $($selected.Count)" -ForegroundColor Cyan
Write-Host ""

foreach ($t in $selected) {
    $triple    = $t.Triple
    $toolchain = $t.Toolchain
    $arch      = $t.Arch
    $libName   = $t.LibName

    Write-Host "[$toolchain/$Config/$arch] cargo build --target $triple ..." -ForegroundColor Yellow

    # Cargo пишет warnings в stderr -- временно ослабляем ErrorAction
    $buildArgs = @("build", "--target", $triple) + $cargoFlags
    $prevEAP = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    $output = & cargo @buildArgs 2>&1
    $buildExit = $LASTEXITCODE
    $ErrorActionPreference = $prevEAP
    foreach ($line in $output) {
        Write-Host "  $line"
    }

    if ($buildExit -ne 0) {
        Write-Host "  FAILED" -ForegroundColor Red
        $failed += "$toolchain/$arch"
        continue
    }

    # Исходный артефакт (Join-Path совместимо с PS 5 -- только 2 аргумента)
    $src = [IO.Path]::Combine($root, "target", $triple, $cargoDir, $libName)
    if (-not (Test-Path $src)) {
        Write-Host "  artifact not found: $src" -ForegroundColor Red
        $failed += "$toolchain/$arch"
        continue
    }

    # Целевая директория
    $destDir     = [IO.Path]::Combine($root, "target", $toolchain, $Config, $arch)
    $destInclude = Join-Path $destDir "include"
    New-Item -ItemType Directory -Path $destDir -Force | Out-Null
    New-Item -ItemType Directory -Path $destInclude -Force | Out-Null

    # Копируем артефакт
    Copy-Item -Path $src -Destination $destDir -Force
    $size = [math]::Round((Get-Item $src).Length / 1MB, 1)

    # Копируем заголовки
    $includeDir = Join-Path $root "include"
    foreach ($h in @("xshm.h", "xshm_server.h", "xshm_client.h")) {
        $hp = Join-Path $includeDir $h
        if (Test-Path $hp) {
            Copy-Item -Path $hp -Destination $destInclude -Force
        }
    }

    Write-Host "  OK -> target\$toolchain\$Config\$arch\$libName ($size MB)" -ForegroundColor Green
}

Write-Host ""
if ($failed.Count -gt 0) {
    Write-Host "FAILED: $($failed -join ', ')" -ForegroundColor Red
    exit 1
} else {
    Write-Host "All builds succeeded." -ForegroundColor Green
    Write-Host ""
    Write-Host "Output:" -ForegroundColor Cyan
    foreach ($t in $selected) {
        $p = [IO.Path]::Combine("target", $t.Toolchain, $Config, $t.Arch, $t.LibName)
        Write-Host "  $p"
    }
    exit 0
}
