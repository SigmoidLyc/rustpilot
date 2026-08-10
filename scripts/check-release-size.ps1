param(
    [string]$ProgramPath = "",
    [string]$InstallerPath = "",
    [double]$LimitMb = 5
)

$ErrorActionPreference = "Stop"

function Resolve-DefaultPath([string]$Path, [string]$Fallback) {
    if ($Path) {
        return [System.IO.Path]::GetFullPath($Path)
    }
    return [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot $Fallback))
}

function Get-SizeBytes([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "Required artifact was not found: $Path"
    }
    return (Get-Item -LiteralPath $Path).Length
}

function Format-Megabytes([long]$Bytes) {
    return "{0:N2} MB" -f ($Bytes / 1MB)
}

$program = Resolve-DefaultPath $ProgramPath "..\.runtime\cargo-target\release\rustpilot.exe"
$installer = Resolve-DefaultPath $InstallerPath "..\.runtime\cargo-target\release\bundle\nsis\RustPilot_0.1.0_x64-setup.exe"
$limitBytes = [long][Math]::Ceiling($LimitMb * 1MB)

$programBytes = Get-SizeBytes $program
$installerBytes = Get-SizeBytes $installer

Write-Output ("program:   {0} ({1})" -f $program, (Format-Megabytes $programBytes))
Write-Output ("installer: {0} ({1})" -f $installer, (Format-Megabytes $installerBytes))
Write-Output ("limit:     {0:N2} MB" -f $LimitMb)

$violations = @()
if ($programBytes -gt $limitBytes) {
    $violations += "program exceeds the $LimitMb MB gate"
}
if ($installerBytes -gt $limitBytes) {
    $violations += "installer exceeds the $LimitMb MB gate"
}
if ($violations.Count -gt 0) {
    throw ($violations -join "; ")
}

Write-Output "release size gate: PASS"
