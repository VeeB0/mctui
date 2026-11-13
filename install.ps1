# Установка / обновление pechka (Windows 10/11, PowerShell):
#   irm https://github.com/VeeB0/pechka/releases/latest/download/install.ps1 | iex
# Настройки (%APPDATA%\pechka\servers.toml) установка не трогает.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

function Get-Setting($Name) {
    # PECHKA_*, а если не задана — прежняя MCTUI_* (программа раньше называлась mctui).
    $v = [Environment]::GetEnvironmentVariable("PECHKA_$Name")
    if (-not $v) { $v = [Environment]::GetEnvironmentVariable("MCTUI_$Name") }
    $v
}
$Repo = if (Get-Setting 'REPO') { Get-Setting 'REPO' } else { 'VeeB0/pechka' }
$OldDir = Join-Path $env:LOCALAPPDATA 'Programs\mctui'
$Dir = Get-Setting 'INSTALL_DIR'
if (-not $Dir) {
    # Старая установка: ставим в ту же папку, она уже в PATH.
    $Dir = if (Test-Path (Join-Path $OldDir 'mctui.exe')) { $OldDir } else { Join-Path $env:LOCALAPPDATA 'Programs\pechka' }
}
$Version = Get-Setting 'VERSION'
$Base = if ($Version) { "https://github.com/$Repo/releases/download/$Version" } else { "https://github.com/$Repo/releases/latest/download" }

if (-not [Environment]::Is64BitOperatingSystem) { throw 'Нужна 64-битная Windows' }
$Asset = 'pechka-x86_64-pc-windows-msvc.exe'   # на Windows ARM работает через эмуляцию x64

New-Item -ItemType Directory -Force -Path $Dir | Out-Null
$Tmp = Join-Path $env:TEMP "pechka-$PID.exe"
Write-Host "› Скачиваю $Asset…" -ForegroundColor Green
Invoke-WebRequest -UseBasicParsing "$Base/$Asset" -OutFile $Tmp
$Expected = ((Invoke-WebRequest -UseBasicParsing "$Base/$Asset.sha256").Content -as [string]).Trim().Split(' ')[0].ToLower()
$Actual = (Get-FileHash $Tmp -Algorithm SHA256).Hash.ToLower()
if ($Expected -ne $Actual) { Remove-Item $Tmp -Force; throw 'Контрольная сумма не совпала — файл повреждён' }

$Exe = Join-Path $Dir 'pechka.exe'
if (Test-Path $Exe) {
    # Запущенный exe нельзя перезаписать, но можно переименовать.
    $Old = "$Exe.old"
    Remove-Item $Old -Force -ErrorAction SilentlyContinue
    Rename-Item $Exe $Old -ErrorAction SilentlyContinue
}
Move-Item $Tmp $Exe -Force
Write-Host "› Установлено: $Exe ($(& $Exe --version))" -ForegroundColor Green

# Программа раньше называлась mctui: mctui.exe убираем, команда mctui остаётся синонимом (mctui.cmd).
$OldExe = Join-Path $Dir 'mctui.exe'
if (Test-Path $OldExe) {
    Remove-Item "$OldExe.old" -Force -ErrorAction SilentlyContinue
    Rename-Item $OldExe "$OldExe.old" -ErrorAction SilentlyContinue
    Set-Content -Path (Join-Path $Dir 'mctui.cmd') -Value '@"%~dp0pechka.exe" %*' -Encoding ASCII
    Write-Host '› mctui теперь называется pechka; команда mctui оставлена синонимом.' -ForegroundColor Green
}

$UserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not (($UserPath -split ';') -contains $Dir)) {
    $New = if ([string]::IsNullOrEmpty($UserPath)) { $Dir } else { "$UserPath;$Dir" }
    [Environment]::SetEnvironmentVariable('Path', $New, 'User')
    $env:Path = "$env:Path;$Dir"
    Write-Host "› Добавил $Dir в PATH. Откройте новое окно терминала." -ForegroundColor Green
}
Write-Host '› Запуск: pechka    Обновление: pechka update' -ForegroundColor Green
