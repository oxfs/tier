param(
    [Parameter(Mandatory,ParameterSetName='Test')][string]$Share,
    [Parameter(Mandatory,ParameterSetName='Writer')][string]$HoldFile
)
$ErrorActionPreference = 'Stop'
if ($PSCmdlet.ParameterSetName -eq 'Writer') {
    $handle=[IO.File]::Open($HoldFile,'Open','ReadWrite',([IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete))
    try {
        [Console]::WriteLine('OPEN')
        Start-Sleep -Seconds 45
    } finally {$handle.Dispose()}
    return
}
$folder=Join-Path $Share ("tier-acceptance-" + [Guid]::NewGuid().ToString('N'))
[IO.Directory]::CreateDirectory($folder) | Out-Null
try {
    $target=Join-Path $folder '照片.txt'
    $temp=Join-Path $folder '照片.tmp'
    $open=[IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete
    $writer=[IO.File]::Open($target,[IO.FileMode]::Create,[IO.FileAccess]::ReadWrite,$open)
    try {
        $writer.Write([byte[]](1,2,3,4),0,4)
        $writer.Flush($true)
        $writer.Position=1
        $writer.WriteByte(9)
        $writer.Flush($true)
        $other=$null
        try { $other=[IO.File]::Open($target,'Open','ReadWrite',$open) } catch [IO.IOException] {} catch [UnauthorizedAccessException] {}
        if ($null -ne $other) { $other.Dispose(); throw 'second writer was not rejected' }
    } finally {$writer.Dispose()}
    if ([Convert]::ToHexString([IO.File]::ReadAllBytes($target)) -ne '01090304'){throw 'partial write mismatch'}
    $reader=[IO.File]::Open($target,'Open','Read',$open)
    try {
        [IO.File]::WriteAllBytes($temp,[byte[]](7,8,9))
        $denied=$false
        try {[IO.File]::Move($temp,$target,$true)} catch [UnauthorizedAccessException] {$denied=$true} catch [IO.IOException] {$denied=$true}
        if (!$denied -or $reader.ReadByte() -ne 1){throw 'expected the open Windows destination to be preserved'}
    } finally {$reader.Dispose()}
    [IO.File]::Move($temp,$target,$true)
    if ([Convert]::ToHexString([IO.File]::ReadAllBytes($target)) -ne '070809'){throw 'replacement contents incorrect'}
    $info=[Diagnostics.ProcessStartInfo]::new((Get-Process -Id $PID).Path)
    $info.UseShellExecute=$false
    $info.CreateNoWindow=$true
    $info.RedirectStandardOutput=$true
    foreach ($argValue in @('-NoProfile','-NonInteractive','-File',$PSCommandPath,'-HoldFile',$target)) {
        $info.ArgumentList.Add($argValue)
    }
    $client=[Diagnostics.Process]::Start($info)
    try {
        if ($client.StandardOutput.ReadLineAsync().WaitAsync([TimeSpan]::FromSeconds(15)).GetAwaiter().GetResult() -ne 'OPEN') {
            throw 'child writer did not open the file'
        }
    } finally {
        if (!$client.HasExited) {$client.Kill()}
        $client.WaitForExit()
        $client.Dispose()
    }
    $timer=[Diagnostics.Stopwatch]::StartNew()
    while ($true) {
        try {
            $writer=[IO.File]::Open($target,'Open','ReadWrite',$open)
            $writer.Dispose()
            break
        } catch {
            if ($timer.Elapsed.TotalSeconds -ge 10) {throw}
            Start-Sleep -Milliseconds 100
        }
    }
    [IO.File]::Delete($target)
    'PASS Windows SMB create, partial write, flush, writer conflict, open-target protection, replace, client exit, reopen, delete'
} finally {
    if ([IO.Directory]::Exists($folder)){ [IO.Directory]::Delete($folder,$true) }
}
