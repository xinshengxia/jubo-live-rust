[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Exe,
    [Parameter(Mandatory)][string]$Mpv,
    [Parameter(Mandatory)][string]$EvidenceDirectory,
    [string]$Target='huya:660000',
    [string]$CredentialDirectory,
    [switch]$Windowed,
    [switch]$KeepOpen,
    [switch]$SyntheticOverlay
)
$ErrorActionPreference='Stop'
$root=[IO.Path]::GetFullPath($EvidenceDirectory)
New-Item -ItemType Directory -Path $root -Force|Out-Null
$data=Join-Path $root ('data-'+[guid]::NewGuid().ToString('N').Substring(0,8))
$pipeName='livehub-production-'+[guid]::NewGuid().ToString('N')
$pipePath='\\.\pipe\'+$pipeName
$windowTitle='Rust Production Check '+$pipeName.Substring($pipeName.Length-8)

function Invoke-Launcher([string[]]$Arguments){
    $info=[Diagnostics.ProcessStartInfo]::new($Exe)
    $info.UseShellExecute=$false;$info.CreateNoWindow=$true
    $info.RedirectStandardOutput=$true;$info.RedirectStandardError=$true
    $info.StandardOutputEncoding=[Text.UTF8Encoding]::new($false)
    $info.StandardErrorEncoding=[Text.UTF8Encoding]::new($false)
    $info.ArgumentList.Add('--data-dir');$info.ArgumentList.Add($data)
    foreach($argument in $Arguments){$info.ArgumentList.Add($argument)}
    if($SyntheticOverlay){
        $info.Environment['LIVE_HUB_OVERLAY_FIXTURE']=(Join-Path $root 'overlay-fixture.json')
        $info.Environment['LIVE_HUB_OVERLAY_DIAG_FORCE_SHOW']='1'
    }
    $process=[Diagnostics.Process]::new();$process.StartInfo=$info
    if(-not $process.Start()){throw 'Launcher did not start'}
    $stderr=$process.StandardError.ReadToEndAsync()
    $lines=[Collections.Generic.List[string]]::new();$deadline=[DateTime]::UtcNow.AddSeconds(60);$parsed=$null
    # Read one complete JSON document, not inherited-pipe EOF. A long-lived
    # relay/player may keep an inherited pipe handle open after launcher exit.
    while([DateTime]::UtcNow -lt $deadline){
        $line=$process.StandardOutput.ReadLineAsync()
        if(-not $line.Wait(60000)){break}
        $value=$line.GetAwaiter().GetResult();if($null -eq $value){break}
        $lines.Add($value)
        try{$parsed=($lines -join "`n")|ConvertFrom-Json -ErrorAction Stop;break}catch{}
    }
    if($null -eq $parsed){
        if(-not $process.HasExited){$process.Kill()}
        $detail=if($stderr.IsCompleted){$stderr.GetAwaiter().GetResult()}else{'no complete JSON receipt'}
        throw ('Production launcher failed: '+$detail)
    }
    if(-not $process.WaitForExit(5000)){$process.Kill();throw 'One-shot production launcher did not exit after its receipt'}
    if($process.ExitCode -ne 0){throw 'Production launcher returned a failure exit code'}
    $process.Dispose();$parsed
}

if($SyntheticOverlay){
    $events=@()
    for($i=0;$i -lt 40;$i++){
        $events+=@{SuperChat=@{id=('synthetic-'+$i);user='合成验收';text='SC 覆盖层显示与穿透测试';price_cents=3000;duration_secs=8}}
        $events+=@{Message=@{text='Rust 生产链路 · 合成弹幕';user='合成用户';color=16777215;mode='Scroll';font_size=27;id=('message-'+$i)}}
    }
    @{initial_delay_ms=3000;interval_ms=1000;events=$events}|ConvertTo-Json -Depth 8|Set-Content -LiteralPath (Join-Path $root 'overlay-fixture.json') -Encoding utf8
}
$inputConfig=Join-Path $root 'acceptance-input.conf'
'MBTN_LEFT cycle pause'|Set-Content -LiteralPath $inputConfig -Encoding utf8
$arguments=@('configure-players','--mpv',$Mpv)
$options=@('--no-config','--mute=yes','--ao=null','--focus-on=never',('--input-ipc-server='+$pipePath),('--title='+$windowTitle),('--input-conf='+$inputConfig))
if($Windowed){$options+='--geometry=960x540+80+80'}else{$options+=@('--vo=null','--force-window=no')}
foreach($option in $options){$arguments+=('--mpv-arg='+$option)}
$null=Invoke-Launcher $arguments
$play=@('play',$Target,'--player','mpv','--with-overlay')
if($CredentialDirectory){$play+=@('--credential-dir',$CredentialDirectory)}
$receipt=Invoke-Launcher $play
$receipt|ConvertTo-Json -Depth 5|Set-Content -LiteralPath (Join-Path $root 'owned-processes.local.json') -Encoding utf8

$client=$null;$writer=$null;$reader=$null;$requestId=0
function Send-Ipc([object[]]$Command){
    $script:requestId++
    $writer.WriteLine((@{command=$Command;request_id=$script:requestId}|ConvertTo-Json -Compress -Depth 6));$writer.Flush()
    for($i=0;$i -lt 300;$i++){
        $line=$reader.ReadLineAsync();if(-not $line.Wait(5000)){throw 'mpv IPC response timed out'}
        $text=$line.GetAwaiter().GetResult();if($null -eq $text){throw 'mpv IPC ended'}
        $message=$text|ConvertFrom-Json
        if($message.request_id -eq $script:requestId){return $message}
    }
    throw 'Too many mpv IPC events'
}
function Get-Ipc([string]$Name){$response=Send-Ipc @('get_property',$Name);if($response.error -eq 'success'){return $response.data};return $null}
try{
    $deadline=[DateTime]::UtcNow.AddSeconds(15)
    while([DateTime]::UtcNow -lt $deadline){
        $candidate=[IO.Pipes.NamedPipeClientStream]::new('.',$pipeName,[IO.Pipes.PipeDirection]::InOut)
        try{$candidate.Connect(500);$client=$candidate;break}catch{$candidate.Dispose();Start-Sleep -Milliseconds 150}
    }
    if(-not $client){throw 'The production player did not expose its requested IPC pipe'}
    $writer=[IO.StreamWriter]::new($client,[Text.UTF8Encoding]::new($false),1024,$true);$writer.AutoFlush=$true
    $reader=[IO.StreamReader]::new($client,[Text.Encoding]::UTF8,$false,1024,$true)
    $deadline=[DateTime]::UtcNow.AddSeconds(25);$first=$null
    while([DateTime]::UtcNow -lt $deadline){$first=Get-Ipc 'time-pos';if($null -ne $first){break};Start-Sleep -Milliseconds 200}
    if($null -eq $first){throw 'No decoded playback progress on the production URL'}
    $uri=[uri](Get-Ipc 'path');Start-Sleep -Seconds 3;$last=Get-Ipc 'time-pos'
    if($SyntheticOverlay){Start-Sleep -Seconds 3}
    if($null -eq $last -or [double]$last-[double]$first -lt 1.0){throw 'Production media clock did not advance'}
    $relayStats=$null
    if($Target -notlike 'bili:*' -or $uri.IsLoopback){
        if(-not $uri.IsLoopback -or $uri.AbsolutePath -ne '/live.flv'){throw 'Production acceptance unexpectedly bypassed the Rust relay'}
        $token=@($uri.Query.TrimStart('?') -split '&'|Where-Object{$_ -like 't=*'})[0].Substring(2)
        $statusUrl='http://127.0.0.1:'+$uri.Port+'/__livehub/status?t='+$token
        $stats=Invoke-RestMethod -Uri $statusUrl -NoProxy -TimeoutSec 4
        if($stats.forwarded_tags -lt 1 -or $stats.forwarded_bytes -lt 1024){throw 'Relay did not report actual media forwarding'}
        $relayStats=@{forwarded_tags=$stats.forwarded_tags;forwarded_bytes=$stats.forwarded_bytes;reconnects=$stats.reconnects;active_streams=$stats.active_streams}
    } elseif($uri.AbsolutePath -notlike '*.m3u8') {throw 'Bili FLV unexpectedly bypassed its configured recovery relay'}
    $report=@{target=$Target;production_launch=$true;windowed=[bool]$Windowed;synthetic_overlay=[bool]$SyntheticOverlay;player_pid=$receipt.player_pid;overlay_pid=$receipt.overlay_pid;window_title=$windowTitle;ipc_pipe=$pipeName;media_host=$uri.Host;media_port=$uri.Port;media_path=$uri.AbsolutePath;clock_delta=([double]$last-[double]$first);video_format=(Get-Ipc 'video-format');width=(Get-Ipc 'width');height=(Get-Ipc 'height');relay=$relayStats;ok=$true}
    $report|ConvertTo-Json -Depth 6|Set-Content -LiteralPath (Join-Path $root 'production-report.local.json') -Encoding utf8
    $report|ConvertTo-Json -Depth 6
}finally{
    if($writer -and -not $KeepOpen){try{$null=Send-Ipc @('quit')}catch{}}
    if($reader){$reader.Dispose()}if($writer){$writer.Dispose()}if($client){$client.Dispose()}
    if(-not $KeepOpen){$player=Get-Process -Id $receipt.player_pid -ErrorAction SilentlyContinue;if($player){$null=$player.WaitForExit(5000)}}
}
