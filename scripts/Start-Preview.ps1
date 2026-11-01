[CmdletBinding()]
param([string]$DataDirectory)
$ErrorActionPreference='Stop'
$project=Split-Path -Parent $PSScriptRoot
$binary=Join-Path $project 'artifacts\current\live-hub-rust.exe'
if(-not (Test-Path -LiteralPath $binary -PathType Leaf)){throw 'Build the application before launching (artifacts\current missing).'}
if(-not $DataDirectory){$DataDirectory=Join-Path $project '.local\user-data'}
$info=[Diagnostics.ProcessStartInfo]::new($binary)
$info.UseShellExecute=$false
$info.WorkingDirectory=$project
foreach($argument in @('--data-dir',[IO.Path]::GetFullPath($DataDirectory),'gui')){$info.ArgumentList.Add($argument)}
$process=[Diagnostics.Process]::Start($info)
if($null -eq $process){throw 'Application launcher did not start.'}
$process.Dispose()
