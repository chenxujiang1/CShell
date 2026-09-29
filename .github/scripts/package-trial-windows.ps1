param(
    [Parameter(Mandatory = $true)]
    [string]$BinaryDirectory
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repository = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$binaryPath = [System.IO.Path]::GetFullPath((Join-Path $repository $BinaryDirectory))
$repositoryPrefix = $repository.TrimEnd([System.IO.Path]::DirectorySeparatorChar) + [System.IO.Path]::DirectorySeparatorChar
if (-not $binaryPath.StartsWith($repositoryPrefix, [System.StringComparison]::OrdinalIgnoreCase)) {
    throw 'The binary directory must stay inside the repository.'
}

$gui = Join-Path $binaryPath 'cshell-gui.exe'
$daemon = Join-Path $binaryPath 'cshelld.exe'
foreach ($binary in @($gui, $daemon)) {
    if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) {
        throw "Missing trial binary: $binary"
    }
}

$commit = (& git -C $repository rev-parse --short=12 HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or $commit -notmatch '^[0-9a-f]{12}$') {
    throw 'Unable to identify the source commit for the trial archive.'
}

$outputDirectory = Join-Path $repository 'artifacts\trial'
New-Item -ItemType Directory -Force -Path $outputDirectory | Out-Null
$archiveName = "CShell-trial-windows-x64-$commit.zip"
$archivePath = Join-Path $outputDirectory $archiveName

Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem
$stream = [System.IO.File]::Open($archivePath, [System.IO.FileMode]::Create)
try {
    $archive = [System.IO.Compression.ZipArchive]::new(
        $stream,
        [System.IO.Compression.ZipArchiveMode]::Create,
        $false,
        [System.Text.Encoding]::UTF8
    )
    try {
        [System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
            $archive, $gui, 'CShell.exe', [System.IO.Compression.CompressionLevel]::Optimal
        ) | Out-Null
        [System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
            $archive, $daemon, 'cshelld.exe', [System.IO.Compression.CompressionLevel]::Optimal
        ) | Out-Null

        $instructions = @(
            'CShell 封闭试用'
            ''
            '1. 解压整个 ZIP，保持 CShell.exe 与 cshelld.exe 在同一目录。'
            '2. 双击 CShell.exe。桌面程序会自行启动后台服务。'
            '3. 点击左侧“＋ 新建 SSH”，填写名称、主机、端口、用户名及认证方式，然后保存连接配置。'
            '4. 如使用密码或加密私钥，请在保存连接配置后单独保存凭据。首次连接前，通过可信渠道核对服务器 SHA256 指纹，再确认导入主机密钥。'
            '5. 点击左侧已保存连接旁的“连接”进入终端。'
            ''
            '反馈问题时，请提供操作系统版本、复现步骤和软件版本。请勿发送密码、私钥或私钥口令。'
        ) -join [System.Environment]::NewLine
        $entry = $archive.CreateEntry('README-first-run.txt', [System.IO.Compression.CompressionLevel]::Optimal)
        $writer = [System.IO.StreamWriter]::new($entry.Open(), [System.Text.UTF8Encoding]::new($false))
        try {
            $writer.Write($instructions)
        } finally {
            $writer.Dispose()
        }
    } finally {
        $archive.Dispose()
    }
} finally {
    $stream.Dispose()
}

$check = [System.IO.Compression.ZipFile]::OpenRead($archivePath)
try {
    $names = @($check.Entries | ForEach-Object FullName | Sort-Object)
    $expected = @('CShell.exe', 'README-first-run.txt', 'cshelld.exe') | Sort-Object
    if (($names -join '|') -ne ($expected -join '|')) {
        throw "Unexpected trial archive contents: $($names -join ', ')"
    }
} finally {
    $check.Dispose()
}

$hash = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
"$hash  $archiveName" | Set-Content -LiteralPath "$archivePath.sha256" -Encoding ascii
Write-Output "Trial archive: $archivePath"
Write-Output "SHA-256: $hash"
