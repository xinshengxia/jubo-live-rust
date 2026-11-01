param([string]$ToolchainRoot = $env:LIVE_HUB_RUST_TOOLCHAIN, [string]$TargetDirectory, [switch]$NoGui)
$ErrorActionPreference = 'Stop'
$cargoArgs = @('test', '--locked', '--offline')
if ($NoGui) { $cargoArgs += '--no-default-features' }
$parameters = @{ ToolchainRoot = $ToolchainRoot }
if ($TargetDirectory) { $parameters.TargetDirectory = $TargetDirectory }
& (Join-Path $PSScriptRoot 'Invoke-Cargo.ps1') @parameters @cargoArgs
