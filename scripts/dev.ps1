$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")
npm.cmd run tauri:dev
