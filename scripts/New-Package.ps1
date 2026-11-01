[CmdletBinding()]
param(
    [string]$ToolchainRoot = $env:LIVE_HUB_RUST_TOOLCHAIN,
    [string]$TargetDirectory,
    [Parameter(Mandatory)][string]$OutputDirectory
)
$ErrorActionPreference = 'Stop'
# Retain the original packaging entry point for older automation callers.
& (Join-Path $PSScriptRoot 'New-Preview.ps1') -ToolchainRoot $ToolchainRoot -TargetDirectory $TargetDirectory -OutputDirectory $OutputDirectory
