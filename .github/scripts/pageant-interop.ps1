$ErrorActionPreference = 'Stop'

if ([string]::IsNullOrWhiteSpace($env:CSHELL_PAGEANT_EXE)) {
    throw 'CSHELL_PAGEANT_EXE must point to a verified Pageant executable'
}

$pageant = [IO.Path]::GetFullPath($env:CSHELL_PAGEANT_EXE)
if (-not (Test-Path -LiteralPath $pageant -PathType Leaf)) {
    throw "Pageant executable was not found: $pageant"
}
if (-not [string]::IsNullOrWhiteSpace($env:RUNNER_TEMP)) {
    $runnerTemp = [IO.Path]::GetFullPath($env:RUNNER_TEMP)
    $expectedPrefix = $runnerTemp.TrimEnd([IO.Path]::DirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
    if (-not $pageant.StartsWith($expectedPrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Pageant executable is outside RUNNER_TEMP: $pageant"
    }
}

$existingPageant = Get-Process -Name pageant -ErrorAction SilentlyContinue
if ($null -ne $existingPageant) {
    throw 'refusing to run Pageant interoperability test while another Pageant process is active'
}

$pageantProcess = $null
try {
    $version = (Get-Item -LiteralPath $pageant).VersionInfo.ProductVersion
    Write-Output "PuTTY Pageant $version"
    $pageantProcess = Start-Process -FilePath $pageant -WindowStyle Hidden -PassThru

    Start-Sleep -Milliseconds 250
    $pageantProcess.Refresh()
    if ($pageantProcess.HasExited) {
        throw "Pageant exited during startup with code $($pageantProcess.ExitCode)"
    }

    $env:CSHELL_PAGEANT_INTEROP = '1'
    cargo test -p cshell-ssh --lib tests::real_pageant_backend_signature_host_key_pty_and_exec_round_trip --all-features --locked -- --exact --test-threads=1
    if ($LASTEXITCODE -ne 0) {
        throw "Pageant interoperability test failed with exit code $LASTEXITCODE"
    }
} finally {
    Remove-Item Env:CSHELL_PAGEANT_INTEROP -ErrorAction SilentlyContinue

    $ownedPageant = Get-Process -Name pageant -ErrorAction SilentlyContinue |
        Where-Object {
            $_.Path -and
            ([IO.Path]::GetFullPath($_.Path)).Equals($pageant, [StringComparison]::OrdinalIgnoreCase)
        }
    foreach ($process in $ownedPageant) {
        Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
        $process.WaitForExit(5000) | Out-Null
        $process.Dispose()
    }
    if ($null -ne $pageantProcess) {
        $pageantProcess.Dispose()
    }
}
