[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Path,
    [string]$ToolchainRoot = $env:LIVE_HUB_RUST_TOOLCHAIN,
    [string[]]$AdditionalPrivatePrefix = @()
)
$ErrorActionPreference = 'Stop'
& (Join-Path $PSScriptRoot 'Test-Preview.ps1') -Path $Path -ToolchainRoot $ToolchainRoot -AdditionalPrivatePrefix $AdditionalPrivatePrefix
