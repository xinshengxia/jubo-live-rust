[CmdletBinding()]
param(
    # Release exe under test (GUI-subsystem release build; CLI path is used).
    [string]$Exe = '',
    [string]$TargetDirectory = $env:LIVE_HUB_BUILD_DIR,
    # Accepts either a real string array or one semicolon/comma separated string
    # (pwsh -File passes every argument as a single string).
    [string[]]$Rooms = @('bili:7777', 'huya:660000', 'douyu:952595', 'douyin:723910165684'),
    # Bounded real-network window per room per round. The 24h acceptance is
    # documented as 4 rooms x 1800s re-run; this script defaults to the same
    # per-room window and allows a shorter bounded run for a time box.
    [int]$Seconds = 1800,
    [int]$Rounds = 1,
    [int]$GapSeconds = 3,
    [string]$OutputDirectory = '',
    # Isolated data roots: never touch the user's LiveHubRust directory.
    [string]$RunRoot = ''
)
$ErrorActionPreference = 'Stop'
$repoPath = Split-Path -Parent $PSScriptRoot
if (-not $OutputDirectory) { $OutputDirectory = Join-Path $repoPath 'artifacts/longrun-danmaku' }
if (-not $RunRoot) { $RunRoot = Join-Path $env:TEMP ('livehub-danmaku-' + [Guid]::NewGuid().ToString('N')) }
$roomList = @($Rooms | ForEach-Object { $_ -split '[;,]' } | ForEach-Object { $_.Trim() } | Where-Object { $_ })
if (-not $roomList.Count) { throw '-Rooms produced no usable room targets' }
$Rooms = $roomList
if (-not $Exe) {
    $candidate = Join-Path $TargetDirectory 'release/live-hub-rust.exe'
    if (Test-Path -LiteralPath $candidate -PathType Leaf) { $Exe = $candidate }
    else { throw 'Provide -Exe <release live-hub-rust.exe>; no release binary found under -TargetDirectory.' }
}
if (-not (Test-Path -LiteralPath $Exe -PathType Leaf)) { throw ('-Exe not found: ' + $Exe) }
if ($Seconds -lt 15) { throw '-Seconds must be at least 15' }
if ($Rounds -lt 1) { throw '-Rounds must be at least 1' }
New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
New-Item -ItemType Directory -Force -Path $RunRoot | Out-Null
$startedAt = Get-Date
$summary = [ordered]@{
    schema = 'livehub-rust-danmaku-longrun/1'
    startedUtc = $startedAt.ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
    exe = [IO.Path]::GetFullPath($Exe)
    secondsPerRun = $Seconds
    rounds = $Rounds
    rooms = @($Rooms)
    runRoot = [IO.Path]::GetFullPath($RunRoot)
    runs = @()
    notes = @(
        '真实网络、无账号、无播放器窗口：只运行 danmaku-probe 的 CLI 路径。',
        '每房间使用独立 --data-dir 隔离目录，不读写用户数据。',
        '24h 验收等价做法：4 房间 x 1800s/房间，重复 8 轮（-Rounds 8）；本任务时间盒用较短 -Seconds + 少量 -Rounds 记录稳定性趋势。'
    )
}
function Get-Median([double[]]$Values) {
    if (-not $Values -or $Values.Count -eq 0) { return $null }
    $sorted = @($Values | Sort-Object)
    return $sorted[[int][Math]::Floor($sorted.Count / 2)]
}
function Invoke-Probe([string]$Room, [int]$Round, [int]$Index) {
    $safeName = ($Room -replace '[^A-Za-z0-9._-]', '_') + '-r' + $Round
    $dataDirectory = Join-Path $RunRoot ($safeName + '-data')
    New-Item -ItemType Directory -Force -Path $dataDirectory | Out-Null
    $stdoutPath = Join-Path $OutputDirectory ($safeName + '.stdout.json')
    $stderrPath = Join-Path $OutputDirectory ($safeName + '.stderr.log')
    $arguments = @('--data-dir', $dataDirectory, 'danmaku-probe', $Room, '--seconds', [string]$Seconds)
    $timer = [System.Diagnostics.Stopwatch]::StartNew()
    $process = Start-Process -FilePath $Exe -ArgumentList $arguments -PassThru -NoNewWindow -RedirectStandardOutput $stdoutPath -RedirectStandardError $stderrPath
    $peakWorkingSet = 0L
    $peakHandles = 0
    $deadline = (Get-Date).AddSeconds($Seconds + 180)
    while (-not $process.HasExited) {
        try {
            $process.Refresh()
            $peakWorkingSet = [Math]::Max($peakWorkingSet, $process.PeakWorkingSet64)
            $peakHandles = [Math]::Max($peakHandles, $process.HandleCount)
        } catch { break }
        if ((Get-Date) -gt $deadline) {
            try { $process.Kill() } catch { }
            break
        }
        Start-Sleep -Seconds 2
    }
    $process.WaitForExit()
    $timer.Stop()
    try { $peakWorkingSet = [Math]::Max($peakWorkingSet, $process.PeakWorkingSet64) } catch { }
    $exitCode = $process.ExitCode
    $stdout = if (Test-Path -LiteralPath $stdoutPath) { [string](Get-Content -LiteralPath $stdoutPath -Raw) } else { '' }
    if ($null -eq $stdout) { $stdout = '' }
    $parsed = $null
    $start = $stdout.IndexOf('{')
    if ($start -ge 0) {
        try { $parsed = $stdout.Substring($start).Trim() | ConvertFrom-Json } catch { $parsed = $null }
    }
    $messages = 0
    $superChat = 0
    $statuses = 0
    if ($parsed -and $parsed.counts) {
        foreach ($property in $parsed.counts.PSObject.Properties) {
            if ($property.Name -eq 'message') { $messages = [int]$property.Value }
            if ($property.Name -eq 'super_chat') { $superChat = [int]$property.Value }
            if ($property.Name -eq 'status') { $statuses = [int]$property.Value }
        }
    }
    # connect_result is {"Ok": null} on a healthy stop or {"Err": "..."} after
    # a failed connection; only the Err property is an error.
    $connectError = $null
    if ($parsed -and $parsed.connect_result -and (@($parsed.connect_result.PSObject.Properties.Name) -contains 'Err')) {
        $connectError = [string]$parsed.connect_result.Err
    }
    $state = 'ok'
    if ($exitCode -ne 0) { $state = 'failed' }
    elseif ($messages -le 0) { $state = 'no-messages' }
    elseif ($connectError) { $state = 'connect-ended-with-error' }
    return [ordered]@{
        room = $Room
        round = $Round
        index = $Index
        state = $state
        exitCode = $exitCode
        wallSeconds = [Math]::Round($timer.Elapsed.TotalSeconds, 1)
        messages = $messages
        superChat = $superChat
        statusEvents = $statuses
        uniqueStatuses = if ($parsed) { @($parsed.statuses).Count } else { 0 }
        connectResult = $connectError
        peakWorkingSetMB = [Math]::Round($peakWorkingSet / 1MB, 2)
        peakHandles = $peakHandles
        stdout = [IO.Path]::GetFullPath($stdoutPath)
        stderr = [IO.Path]::GetFullPath($stderrPath)
    }
}
$index = 0
for ($round = 1; $round -le $Rounds; $round++) {
    foreach ($room in $Rooms) {
        $index++
        Write-Host ('== [' + $index + '] ' + $room + ' round ' + $round + ' for ' + $Seconds + 's') -ForegroundColor Cyan
        try {
            $run = Invoke-Probe -Room $room -Round $round -Index $index
        } catch {
            $run = [ordered]@{ room = $room; round = $round; index = $index; state = 'harness-error'; exitCode = -1; error = $_.Exception.Message }
        }
        $summary.runs += $run
        Write-Host ('   ' + $run.state + ' exit=' + $run.exitCode + ' messages=' + $run.messages + ' wall=' + $run.wallSeconds + 's')
        Start-Sleep -Seconds $GapSeconds
    }
}
$summary.durationSeconds = [Math]::Round(((Get-Date) - $startedAt).TotalSeconds, 1)
$summary.totals = [ordered]@{
    runs = $summary.runs.Count
    ok = @($summary.runs | Where-Object { $_.state -eq 'ok' }).Count
    noMessages = @($summary.runs | Where-Object { $_.state -eq 'no-messages' }).Count
    failed = @($summary.runs | Where-Object { $_.state -eq 'failed' }).Count
    harnessErrors = @($summary.runs | Where-Object { $_.state -eq 'harness-error' }).Count
    messages = (@($summary.runs | ForEach-Object { [int]$_.messages }) | Measure-Object -Sum).Sum
    peakWorkingSetMB = Get-Median ([double[]]@($summary.runs | Where-Object { $_.peakWorkingSetMB } | ForEach-Object { $_.peakWorkingSetMB }))
    peakHandles = Get-Median ([double[]]@($summary.runs | Where-Object { $_.peakHandles } | ForEach-Object { $_.peakHandles }))
}
$stamp = $startedAt.ToString('yyyyMMdd-HHmmss')
$jsonPath = Join-Path $OutputDirectory ('danmaku-longrun-' + $stamp + '.json')
$summary | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $jsonPath -Encoding utf8
Copy-Item -LiteralPath $jsonPath -Destination (Join-Path $OutputDirectory 'danmaku-longrun-latest.json') -Force
Write-Host ''
$summary.runs | ForEach-Object {
    Write-Host ($_.room.PadRight(24) + ' ' + $_.state.PadRight(12) + ' messages=' + $_.messages + ' status=' + $_.statusEvents + ' wall=' + $_.wallSeconds + 's')
}
Write-Host ('result -> ' + $jsonPath) -ForegroundColor Green
if ($summary.totals.failed -gt 0 -or $summary.totals.harnessErrors -gt 0) { exit 1 }
exit 0
