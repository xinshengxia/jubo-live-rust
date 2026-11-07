param(
    [string]$Exe = '',
    [string]$Mpv = '',
    [string]$EvidenceDir = '',
    [string[]]$Targets = @('bili:7734200','huya:660000','douyu:317422')
)
$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
if (-not $Exe) {
    if ($env:LIVE_HUB_ACCEPTANCE_EXE) { $Exe = $env:LIVE_HUB_ACCEPTANCE_EXE }
    else { $Exe = Join-Path $repoRoot 'target\debug\live-hub-rust.exe' }
}
if (-not $Mpv) {
    if ($env:LIVE_HUB_MPV) { $Mpv = $env:LIVE_HUB_MPV }
    else { throw 'set -Mpv or LIVE_HUB_MPV to a local mpv.exe (no player is installed by this project)' }
}
if (-not $EvidenceDir) { $EvidenceDir = Join-Path $env:TEMP 'livehub-mpv-evidence' }
New-Item -ItemType Directory -Path $EvidenceDir -Force | Out-Null
if (-not (Test-Path -LiteralPath $Exe)) { throw ('launcher executable not found: ' + $Exe) }
if (-not (Test-Path -LiteralPath $Mpv)) { throw ('mpv executable not found: ' + $Mpv) }
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$OutputEncoding = [System.Text.Encoding]::UTF8

$tmp = Join-Path $env:TEMP ('livehub-mpv-' + [guid]::NewGuid().ToString('N').Substring(0,8))
New-Item -ItemType Directory -Path $tmp -Force | Out-Null
$script:reqId = 0
$script:mpvWriter = $null
$script:mpvReader = $null

function Invoke-CliJson([string[]]$cliArgs) {
    $out = & $Exe --data-dir $tmp @cliArgs 2>&1
    if ($LASTEXITCODE -ne 0) { throw ("CLI failed (" + $LASTEXITCODE + "): " + (($out | Out-String).Trim())) }
    return ((($out | Out-String).Trim()) | ConvertFrom-Json)
}

function Connect-MpvPipe([string]$name, [int]$timeoutMs = 8000) {
    $deadline = [DateTime]::UtcNow.AddMilliseconds($timeoutMs)
    while ([DateTime]::UtcNow -lt $deadline) {
        $client = New-Object System.IO.Pipes.NamedPipeClientStream('.', $name, [System.IO.Pipes.PipeDirection]::InOut, [System.IO.Pipes.PipeOptions]::None)
        try { $client.Connect(500); return $client } catch { $client.Dispose(); Start-Sleep -Milliseconds 150 }
    }
    throw "mpv IPC pipe '$name' was not created"
}

function Start-MpvIpc([System.IO.Pipes.NamedPipeClientStream]$client) {
    $script:reqId = 0
    $script:mpvWriter = New-Object System.IO.StreamWriter($client)
    $script:mpvWriter.AutoFlush = $true
    $script:mpvReader = New-Object System.IO.StreamReader($client)
}

function Send-Mpv([string]$command, [object[]]$arguments = @(), [switch]$NoWait) {
    $script:reqId++
    $body = @($command)
    if ($arguments.Count -gt 0) { $body = $body + $arguments }
    $request = @{ command = $body; request_id = $script:reqId } | ConvertTo-Json -Compress -Depth 8
    $script:mpvWriter.WriteLine($request)
    $script:mpvWriter.Flush()
    if ($NoWait) { return $null }
    for ($i = 0; $i -lt 2000; $i++) {
        $line = $script:mpvReader.ReadLine()
        if ($null -eq $line) { throw 'mpv closed its IPC pipe' }
        try { $message = $line | ConvertFrom-Json } catch { continue }
        if ($message.PSObject.Properties.Name -contains 'request_id' -and $message.request_id -eq $script:reqId) { return $message }
    }
    throw "no IPC response for request $($script:reqId)"
}

function Get-MpvProperty([string]$name) {
    $response = Send-Mpv 'get_property' @($name)
    if ($response.error -ne 'success') { return [pscustomobject]@{ name = $name; error = $response.error; data = $null } }
    return [pscustomobject]@{ name = $name; error = 'success'; data = $response.data }
}

function Wait-MpvProperty([string]$name, [int]$timeoutMs = 15000) {
    $deadline = [DateTime]::UtcNow.AddMilliseconds($timeoutMs)
    $attempts = 0
    while ([DateTime]::UtcNow -lt $deadline) {
        $attempts++
        $response = Send-Mpv 'get_property' @($name)
        if (($response.error -eq 'success') -and ($null -ne $response.data)) { return [pscustomobject]@{ name = $name; error = 'success'; data = $response.data; attempts = $attempts } }
        Start-Sleep -Milliseconds 250
    }
    return [pscustomobject]@{ name = $name; error = 'timeout'; data = $null; attempts = $attempts }
}

function Quote-Argument([string]$value) {
    if ($value -match '[\s"]') { return '"' + ($value -replace '"', '\"') + '"' }
    return $value
}

$results = @()
foreach ($target in $Targets) {
    $record = [ordered]@{
        target = $target; ok = $false; stage = 'start'
        resolve = $null; plan_arguments = $null; ipc = $null
        stderr_head = $null; error = $null; quit_exited = $false; option_errors = @()
    }
    $mpvProcess = $null
    $client = $null
    $tag = ($target -replace ':', '_')
    try {
        $resolve = Invoke-CliJson @('resolve', $target, '--quality', 'best')
        $record.stage = 'resolved'
        $record.resolve = [ordered]@{ url_host = ([uri]$resolve.url).Host; title = $resolve.title; header_names = @($resolve.headers.PSObject.Properties.Name) }
        $plan = Invoke-CliJson @('plan', $target, '--media-url', $resolve.url, '--executable', $Mpv)
        $planArguments = @($plan.arguments | ForEach-Object { [string]$_ })
        $record.plan_arguments = $planArguments
        $separator = [Array]::IndexOf($planArguments, '--')
        if ($separator -lt 0) { throw 'plan output has no -- media separator' }

        $pipeName = 'livehub-mpv-' + [guid]::NewGuid().ToString('N')
        $pipePath = '\\.\pipe\' + $pipeName
        $final = @('--vo=null', '--ao=null', '--no-config', '--really-quiet', '--idle=no', ('--input-ipc-server=' + $pipePath))
        for ($i = 0; $i -lt $separator; $i++) {
            $argument = $planArguments[$i]
            # This acceptance harness owns audio/video output (--ao=null/--vo=null).
            # Drop the launcher's default audio-output policy so it cannot override
            # the harness choice; the launcher-only OpenAL env is not set here.
            if ($argument -like '--ao=*' -or $argument -like '--audio-device=*') { continue }
            if ($argument -eq '--force-window=immediate') { $final += '--force-window=no' } else { $final += $argument }
        }
        if ($resolve.headers.Referer) { $final += ('--referrer=' + $resolve.headers.Referer) }
        $userAgent = $resolve.headers.PSObject.Properties['User-Agent']
        if ($userAgent) { $final += ('--user-agent=' + $userAgent.Value) }
        $final += '--'
        $final += [string]$resolve.url
        $commandLine = (($final | ForEach-Object { Quote-Argument $_ }) -join ' ')
        $record.stage = 'spawned'
        $stderrFile = Join-Path $tmp ('mpv-' + $tag + '.stderr.txt')
        $stdoutFile = Join-Path $tmp ('mpv-' + $tag + '.stdout.txt')
        $mpvProcess = Start-Process -FilePath $Mpv -ArgumentList $commandLine -PassThru -WindowStyle Hidden -RedirectStandardError $stderrFile -RedirectStandardOutput $stdoutFile
        Start-Sleep -Milliseconds 300
        if ($mpvProcess.HasExited) { throw ("mpv exited immediately with code " + $mpvProcess.ExitCode + '; stderr=' + ((Get-Content -Raw -LiteralPath $stderrFile) -replace '\s+', ' ')) }
        $client = Connect-MpvPipe $pipeName
        Start-MpvIpc $client
        $record.stage = 'ipc-connected'
        $version = Get-MpvProperty 'mpv-version'
        $title = Get-MpvProperty 'media-title'
        $duration = Get-MpvProperty 'duration'
        $width = Get-MpvProperty 'width'
        $height = Get-MpvProperty 'height'
        # Live streams need a moment before the first frame has decoded: poll the
        # video parameters instead of sampling them once right after IPC connect.
        $format = Wait-MpvProperty 'video-format' 15000
        $videoWidth = Wait-MpvProperty 'video-params/w' 15000
        $videoHeight = Wait-MpvProperty 'video-params/h' 15000
        $startedAt = Wait-MpvProperty 'time-pos' 15000
        Start-Sleep -Milliseconds 2500
        $stoppedAt = Get-MpvProperty 'time-pos'
        $eof = Get-MpvProperty 'eof'
        $paused = Get-MpvProperty 'pause'
        $idle = Get-MpvProperty 'core-idle'
        $idleActive = Get-MpvProperty 'idle-active'
        $advanced = $null
        if (($null -ne $startedAt.data) -and ($null -ne $stoppedAt.data)) { $advanced = [double]$stoppedAt.data - [double]$startedAt.data }
        $record.ipc = [ordered]@{
            mpv_version = $version.data
            media_title = $title.data
            video_format = $format.data
            video_format_attempts = $format.attempts
            video_width = $videoWidth.data
            video_height = $videoHeight.data
            video_width_attempts = $videoWidth.attempts
            time_pos_start_attempts = $startedAt.attempts
            width = $width.data
            height = $height.data
            duration = $duration.data
            time_pos_start = $startedAt.data
            time_pos_end = $stoppedAt.data
            time_advanced = $advanced
            eof = $eof.data
            paused = $paused.data
            core_idle = $idle.data
            idle_active = $idleActive.data
        }
        $record.stage = 'measured'
        Send-Mpv 'quit' @() -NoWait | Out-Null
        $exited = $mpvProcess.WaitForExit(5000)
        if (-not $exited) { Stop-Process -Id $mpvProcess.Id -Force; $exited = $mpvProcess.WaitForExit(3000) }
        $record.quit_exited = $exited
        $stderrText = ''
        if (Test-Path -LiteralPath $stderrFile) { $stderrText = Get-Content -Raw -LiteralPath $stderrFile }
        $record.stderr_head = (($stderrText -split '\r?\n' | Where-Object { $_ -match '\S' } | Select-Object -First 5) -join ' | ')
        $failedOptions = @($stderrText -split '\r?\n' | Where-Object { $_ -match 'unknown option|Error parsing option|option not found|Invalid value' })
        $record.option_errors = $failedOptions
        $record.ok = (
            ($version.error -eq 'success') -and
            ($format.error -eq 'success') -and (-not [string]::IsNullOrEmpty([string]$format.data)) -and
            ($null -ne $videoWidth.data) -and ([int]$videoWidth.data -gt 0) -and
            ($null -ne $videoHeight.data) -and ([int]$videoHeight.data -gt 0) -and
            ($null -ne $advanced) -and ([double]$advanced -gt 1.0) -and
            ($eof.data -ne $true) -and
            ($idleActive.data -eq $false) -and
            ($failedOptions.Count -eq 0) -and ($exited)
        )
        $record.stage = 'done'
    } catch {
        $record.error = ($_.Exception.Message -replace '\s+', ' ')
        $record.ok = $false
    } finally {
        if ($client) { try { $client.Dispose() } catch {} }
        if ($mpvProcess -and -not $mpvProcess.HasExited) { try { Stop-Process -Id $mpvProcess.Id -Force } catch {} }
    }
    $results += [pscustomobject]$record
    Write-Host ("[" + $target + "] ok=" + $record.ok + " stage=" + $record.stage + " error=" + $record.error)
}

$report = [ordered]@{
    kind = 'mpv-headless-decode-acceptance'
    generated_utc = [DateTime]::UtcNow.ToString('o')
    exe = $Exe
    mpv = $Mpv
    results = $results
}
$outPath = Join-Path $EvidenceDir 'mpv_acceptance.json'
$report | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $outPath -Encoding UTF8
Write-Host ("evidence: " + $outPath)
Remove-Item -Recurse -Force -LiteralPath $tmp -ErrorAction SilentlyContinue
