# Установка / обновление mctui (Windows 10/11, PowerShell):
#   irm https://github.com/VeeB0/mctui/releases/latest/download/install.ps1 | iex
# Настройки (%APPDATA%\mctui\servers.toml) установка не трогает.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$Repo = if ($env:MCTUI_REPO) { $env:MCTUI_REPO } else { 'VeeB0/mctui' }
$Dir = if ($env:MCTUI_INSTALL_DIR) { $env:MCTUI_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\mctui' }
$Base = if ($env:MCTUI_VERSION) { "https://github.com/$Repo/releases/download/$($env:MCTUI_VERSION)" } else { "https://github.com/$Repo/releases/latest/download" }

if (-not [Environment]::Is64BitOperatingSystem) { throw 'Нужна 64-битная Windows' }
$Asset = 'mctui-x86_64-pc-windows-msvc.exe'   # на Windows ARM работает через эмуляцию x64

New-Item -ItemType Directory -Force -Path $Dir | Out-Null
$Tmp = Join-Path $env:TEMP "mctui-$PID.exe"
Write-Host "› Скачиваю $Asset…" -ForegroundColor Green
Invoke-WebRequest -UseBasicParsing "$Base/$Asset" -OutFile $Tmp
$Expected = ((Invoke-WebRequest -UseBasicParsing "$Base/$Asset.sha256").Content -as [string]).Trim().Split(' ')[0].ToLower()
$Actual = (Get-FileHash $Tmp -Algorithm SHA256).Hash.ToLower()
if ($Expected -ne $Actual) { Remove-Item $Tmp -Force; throw 'Контрольная сумма не совпала — файл повреждён' }

$Exe = Join-Path $Dir 'mctui.exe'
if (Test-Path $Exe) {
    # Запущенный exe нельзя перезаписать, но можно переименовать.
    $Old = "$Exe.old"
    Remove-Item $Old -Force -ErrorAction SilentlyContinue
    Rename-Item $Exe $Old -ErrorAction SilentlyContinue
}
Move-Item $Tmp $Exe -Force
Write-Host "› Установлено: $Exe ($(& $Exe --version))" -ForegroundColor Green

$UserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not (($UserPath -split ';') -contains $Dir)) {
    $New = if ([string]::IsNullOrEmpty($UserPath)) { $Dir } else { "$UserPath;$Dir" }
    [Environment]::SetEnvironmentVariable('Path', $New, 'User')
    $env:Path = "$env:Path;$Dir"
    Write-Host "› Добавил $Dir в PATH. Откройте новое окно терминала." -ForegroundColor Green
}
Write-Host '› Запуск: mctui    Обновление: mctui update' -ForegroundColor Green
