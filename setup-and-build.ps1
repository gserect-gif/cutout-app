# Run this from inside the bgremover project folder in PowerShell.
# It downloads the AI model (if not already present) and builds the
# production Windows installer in one go.

$ErrorActionPreference = "Stop"

$modelDir = "models"
$modelPath = Join-Path $modelDir "model.onnx"

if (-not (Test-Path $modelPath)) {
    Write-Host "Downloading AI model (224MB, one-time)..." -ForegroundColor Cyan
    New-Item -ItemType Directory -Force -Path $modelDir | Out-Null
    $url = "https://huggingface.co/onnx-community/BiRefNet_lite-ONNX/resolve/main/onnx/model.onnx"
    Invoke-WebRequest -Uri $url -OutFile $modelPath
    Write-Host "Model downloaded." -ForegroundColor Green
} else {
    Write-Host "Model already present, skipping download." -ForegroundColor Green
}

Write-Host "Installing frontend dependencies..." -ForegroundColor Cyan
npm install

Write-Host "Building production app (this compiles Rust from scratch if it's the first build - can take 5-15 minutes)..." -ForegroundColor Cyan
npm run tauri build

Write-Host ""
Write-Host "Done. Look for your installer here:" -ForegroundColor Green
Write-Host "  src-tauri\target\release\bundle\msi\*.msi"
Write-Host "  src-tauri\target\release\bundle\nsis\*-setup.exe"
