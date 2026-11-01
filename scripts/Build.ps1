param([string]$ToolchainRoot = $env:LIVE_HUB_RUST_TOOLCHAIN, [string]$TargetDirectory, [switch]$NoGui, [switch]$Release)
$ErrorActionPreference = 'Stop'
$cargoArgs = @('build', '--locked')
if ($NoGui) { $cargoArgs += '--no-default-features' }
if ($Release) { $cargoArgs += '--release' }
$parameters = @{ ToolchainRoot = $ToolchainRoot }
if ($TargetDirectory) { $parameters.TargetDirectory = $TargetDirectory }
& (Join-Path $PSScriptRoot 'Invoke-Cargo.ps1') @parameters @cargoArgs
