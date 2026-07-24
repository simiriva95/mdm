# Installa MDM: trova/compila l'exe, lo copia, scrive manifest native messaging + registry.
# Doppio click su install.bat, oppure: powershell -ExecutionPolicy Bypass -File install.ps1
$ErrorActionPreference = 'Stop'

# 1) exe: accanto allo script (pacchetto CI) > build locale > cargo build
$exeSrc = Join-Path $PSScriptRoot 'mdm.exe'
if (-not (Test-Path $exeSrc)) { $exeSrc = Join-Path $PSScriptRoot '..\app\target\release\mdm.exe' }
if (-not (Test-Path $exeSrc)) {
    $cargoToml = Join-Path $PSScriptRoot '..\app\Cargo.toml'
    if ((Test-Path $cargoToml) -and (Get-Command cargo -ErrorAction SilentlyContinue)) {
        Write-Host 'Compilo mdm.exe (prima volta: qualche minuto)...'
        cargo build --release --manifest-path $cargoToml
        $exeSrc = Join-Path $PSScriptRoot '..\app\target\release\mdm.exe'
    }
}
if (-not (Test-Path $exeSrc)) { throw 'mdm.exe non trovato e cargo non disponibile. Scarica il pacchetto dalla CI di GitHub (Actions -> mdm-windows).' }

# 2) copia in %LOCALAPPDATA%\MDM
$dest = Join-Path $env:LOCALAPPDATA 'MDM'
New-Item -ItemType Directory -Force -Path $dest | Out-Null
Copy-Item $exeSrc (Join-Path $dest 'mdm.exe') -Force

# 3) manifest native messaging + registry (solo utente corrente, niente admin)
$manifest = Get-Content (Join-Path $PSScriptRoot 'com.sriva.downloader.json') -Raw
$exePath = (Join-Path $dest 'mdm.exe') -replace '\\', '\\'
$manifest = $manifest -replace 'EXE_PATH', $exePath
$manifestPath = Join-Path $dest 'com.sriva.downloader.json'
Set-Content -Path $manifestPath -Value $manifest -Encoding UTF8

$regKey = 'HKCU:\Software\Google\Chrome\NativeMessagingHosts\com.sriva.downloader'
New-Item -Path $regKey -Force | Out-Null
Set-ItemProperty -Path $regKey -Name '(Default)' -Value $manifestPath

Write-Host ''
Write-Host "OK! Installato in $dest"
Write-Host 'Ultimo passo (una volta sola): chrome://extensions -> Developer mode ON -> "Load unpacked" -> cartella extension\'
