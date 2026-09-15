$ErrorActionPreference = 'Stop'

$openSshRoot = Join-Path $env:WINDIR 'System32\OpenSSH'
$sshd = Join-Path $openSshRoot 'sshd.exe'
$sshKeygen = Join-Path $openSshRoot 'ssh-keygen.exe'

if (-not (Test-Path -LiteralPath $sshd)) {
    $capability = Get-WindowsCapability -Online |
        Where-Object Name -Like 'OpenSSH.Server*' |
        Select-Object -First 1
    if ($null -eq $capability) {
        throw 'Windows OpenSSH Server capability was not found'
    }
    if ($capability.State -ne 'Installed') {
        Add-WindowsCapability -Online -Name $capability.Name
    }
}
if (-not (Test-Path -LiteralPath $sshd) -or -not (Test-Path -LiteralPath $sshKeygen)) {
    throw 'Windows OpenSSH Server binaries are unavailable after capability installation'
}

$tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
$interopRoot = Join-Path $tempRoot ("cshell-windows-openssh-{0}" -f [guid]::NewGuid().ToString('N'))
$sshdProcess = $null
New-Item -ItemType Directory -Path $interopRoot | Out-Null

try {
    $hostKey = Join-Path $interopRoot 'host_key'
    $clientKey = Join-Path $interopRoot 'client_key'
    $authorizedKeys = Join-Path $interopRoot 'authorized_keys'
    $configPath = Join-Path $interopRoot 'sshd_config'
    $logPath = Join-Path $interopRoot 'sshd.log'

    & $sshKeygen -q -t ed25519 -N '' -f $hostKey
    if ($LASTEXITCODE -ne 0) { throw 'failed to generate Windows OpenSSH host key' }
    & $sshKeygen -q -t ed25519 -N '' -f $clientKey
    if ($LASTEXITCODE -ne 0) { throw 'failed to generate Windows OpenSSH client key' }
    Copy-Item -LiteralPath "$clientKey.pub" -Destination $authorizedKeys

    $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
    $listener.Start()
    $port = ([Net.IPEndPoint]$listener.LocalEndpoint).Port
    $listener.Stop()

    $hostKeyConfig = $hostKey.Replace('\', '/')
    $authorizedKeysConfig = $authorizedKeys.Replace('\', '/')
    $pidConfig = (Join-Path $interopRoot 'sshd.pid').Replace('\', '/')
    @(
        "Port $port"
        'ListenAddress 127.0.0.1'
        "HostKey `"$hostKeyConfig`""
        "PidFile `"$pidConfig`""
        "AuthorizedKeysFile `"$authorizedKeysConfig`""
        'PasswordAuthentication no'
        'KbdInteractiveAuthentication no'
        'ChallengeResponseAuthentication no'
        'StrictModes no'
        'PermitTTY yes'
        "AllowUsers $env:USERNAME"
        'LogLevel VERBOSE'
    ) | Set-Content -LiteralPath $configPath -Encoding ascii

    $sshdArguments = @('-D', '-e', '-f', $configPath)
    $sshdProcess = Start-Process -FilePath $sshd -ArgumentList $sshdArguments -WindowStyle Hidden -PassThru -RedirectStandardError $logPath

    $ready = $false
    for ($attempt = 0; $attempt -lt 150; $attempt++) {
        $sshdProcess.Refresh()
        if ($sshdProcess.HasExited) { break }
        $probe = [Net.Sockets.TcpClient]::new()
        try {
            $connect = $probe.ConnectAsync([Net.IPAddress]::Loopback, $port)
            if ($connect.Wait(200) -and $probe.Connected) {
                $ready = $true
                break
            }
        } catch {
            # The isolated sshd may still be starting.
        } finally {
            $probe.Dispose()
        }
        Start-Sleep -Milliseconds 100
    }
    if (-not $ready) {
        if (Test-Path -LiteralPath $logPath) { Get-Content -LiteralPath $logPath }
        throw 'Windows OpenSSH Server did not become ready'
    }

    $fingerprintOutput = & $sshKeygen -q -l -E sha256 -f "$hostKey.pub"
    if ($LASTEXITCODE -ne 0) { throw 'failed to calculate Windows OpenSSH host fingerprint' }
    $fingerprint = ($fingerprintOutput -split '\s+')[1]

    $env:CSHELL_BASIC_SSH_INTEROP = '1'
    $env:CSHELL_BASIC_SSH_ADDRESS = "127.0.0.1:$port"
    $env:CSHELL_BASIC_SSH_USERNAME = $env:USERNAME
    $env:CSHELL_BASIC_SSH_HOST_FINGERPRINT = $fingerprint
    $env:CSHELL_BASIC_SSH_PRIVATE_KEY = $clientKey

    $version = (Get-Item -LiteralPath $sshd).VersionInfo.FileVersion
    Write-Output "Windows OpenSSH Server $version"
    cargo test -p cshell-ssh --test basic_ssh_interop --all-features --locked -- --test-threads=1
    if ($LASTEXITCODE -ne 0) {
        if (Test-Path -LiteralPath $logPath) { Get-Content -LiteralPath $logPath }
        throw "Windows OpenSSH interoperability test failed with exit code $LASTEXITCODE"
    }
} finally {
    if ($null -ne $sshdProcess) {
        $sshdProcess.Refresh()
        if (-not $sshdProcess.HasExited) {
            Stop-Process -Id $sshdProcess.Id -Force -ErrorAction SilentlyContinue
            $sshdProcess.WaitForExit(5000) | Out-Null
        }
        $sshdProcess.Dispose()
    }
    Remove-Item Env:CSHELL_BASIC_SSH_INTEROP -ErrorAction SilentlyContinue
    Remove-Item Env:CSHELL_BASIC_SSH_ADDRESS -ErrorAction SilentlyContinue
    Remove-Item Env:CSHELL_BASIC_SSH_USERNAME -ErrorAction SilentlyContinue
    Remove-Item Env:CSHELL_BASIC_SSH_HOST_FINGERPRINT -ErrorAction SilentlyContinue
    Remove-Item Env:CSHELL_BASIC_SSH_PRIVATE_KEY -ErrorAction SilentlyContinue

    $resolvedInteropRoot = [IO.Path]::GetFullPath($interopRoot)
    $expectedPrefix = $tempRoot.TrimEnd([IO.Path]::DirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
    if (-not $resolvedInteropRoot.StartsWith($expectedPrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "refusing to remove unexpected interoperability path: $resolvedInteropRoot"
    }
    if (Test-Path -LiteralPath $resolvedInteropRoot) {
        Remove-Item -LiteralPath $resolvedInteropRoot -Recurse -Force
    }
}
