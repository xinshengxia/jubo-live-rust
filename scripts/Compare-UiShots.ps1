<#
.SYNOPSIS
  Compare C# original launcher screenshots with Rust --ui-shot captures.

.DESCRIPTION
  Wrapper around scripts/compare_ui_shots.py (Pillow + numpy, offline). For every
  page it writes <OutputDirectory>/<page>.png = [reference | rust | 3x amplified
  diff] plus summary.json / summary.md with quantitative differences (MAE,
  changed%, ink ratio, mean colour, native sizes) and per-region text bounding
  boxes taken from the original control tree, so font-size and offset drift are
  measurable.

  Reference discovery per page key: <page>-<theme>-200.png, -100, -150,
  <page>-<theme>.png, <page>.png. Rust side: <page>.png.

  Numbers are a screening signal only: font rasterisation, DPI scaling,
  avatars/live badges (network data) and transient state still differ. Review the
  composite images.

.PARAMETER ReferenceDirectory
  Directory with the C# original captures (notes/reference/original).

.PARAMETER RustDirectory
  Directory produced by scripts/Capture-RustUi.ps1.

.PARAMETER OutputDirectory
  Receives <page>.png composites, summary.json and summary.md
  (e.g. output/ui-compare).

.PARAMETER PythonExe
  Optional interpreter that can import PIL and numpy. When omitted the wrapper
  probes known local environments (conda py313/yolo, Inkscape python) because the
  default miniforge python has no Pillow.

.EXAMPLE
  pwsh -NoProfile -File scripts/Compare-UiShots.ps1 -ReferenceDirectory 'D:\ref' -RustDirectory 'D:\rust' -OutputDirectory 'D:\compare'
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$ReferenceDirectory,
    [Parameter(Mandatory = $true)][string]$RustDirectory,
    [Parameter(Mandatory = $true)][string]$OutputDirectory,
    [ValidateSet('dark', 'light')][string]$Theme = 'dark',
    [int]$MaxWidth = 1240,
    [int]$ChangedThreshold = 24,
    [string]$Pages = '',
    [switch]$AllowMissing,
    [string]$PythonExe
)
$ErrorActionPreference = 'Stop'
$scriptPath = Join-Path $PSScriptRoot 'compare_ui_shots.py'
$ReferenceDirectory = [IO.Path]::GetFullPath($ReferenceDirectory)
$RustDirectory = [IO.Path]::GetFullPath($RustDirectory)
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
foreach ($directory in @($ReferenceDirectory, $RustDirectory)) {
    if (-not (Test-Path -LiteralPath $directory)) { throw "目录不存在：$directory" }
}
New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null

function Test-PythonImaging([string]$candidate) {
    if (-not $candidate) { return $false }
    if (-not (Test-Path -LiteralPath $candidate)) { return $false }
    try {
        & $candidate -c "import PIL, numpy" 2>$null | Out-Null
        return ($LASTEXITCODE -eq 0)
    }
    catch { return $false }
}

$candidates = @()
if ($PythonExe) { $candidates += $PythonExe }
$candidates += @(
    'D:\conda_envs\py313\python.exe',
    'D:\conda_envs\yolo\python.exe',
    'C:\Program Files\Inkscape\bin\python.exe',
    (Get-Command python -ErrorAction SilentlyContinue).Source
) | Where-Object { $_ }
$resolved = $null
foreach ($candidate in $candidates) {
    if (Test-PythonImaging $candidate) { $resolved = $candidate; break }
}
if (-not $resolved) {
    throw '找不到带 Pillow + numpy 的 python。请用 -PythonExe 指定（本机可用 D:\conda_envs\py313\python.exe）。'
}
Write-Host ("[ui-compare] python: " + $resolved)

$arguments = @(
    $scriptPath,
    '--reference', $ReferenceDirectory,
    '--rust', $RustDirectory,
    '--out', $OutputDirectory,
    '--theme', $Theme,
    '--max-width', [string]$MaxWidth,
    '--changed-threshold', [string]$ChangedThreshold
)
if ($Pages) { $arguments += @('--pages', $Pages) }
if ($AllowMissing) { $arguments += '--allow-missing' }

& $resolved @arguments
$code = $LASTEXITCODE
$summary = Join-Path $OutputDirectory 'summary.md'
if (Test-Path -LiteralPath $summary) {
    Write-Host ("[ui-compare] summary: " + $summary)
    Get-Content -LiteralPath $summary | ForEach-Object { Write-Host $_ }
}
if ($code -ne 0) { throw "截图对比失败（exit $code）" }
Write-Host ("[ui-compare] composites: " + $OutputDirectory)
