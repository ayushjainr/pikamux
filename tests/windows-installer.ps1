# Runs only on a disposable Windows CI runner; never pairs or launches agents.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem
$installer = Join-Path (Split-Path $PSScriptRoot -Parent) 'install.ps1'
$testRoot = Join-Path ([IO.Path]::GetTempPath()) ('pika-installer-test-' + [Guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory($testRoot)
$oldLocal = $env:LOCALAPPDATA
$oldPath = $env:PATH
$oldUserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$utf8 = New-Object System.Text.UTF8Encoding($false)

function Check([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw "FAILED: $Message" }
    Write-Host "PASS: $Message"
}

function Reject([scriptblock]$Action, [string]$Pattern) {
    $failure = $null
    try { & $Action } catch { $failure = $_.ToString() }
    Check ($null -ne $failure -and $failure -match $Pattern) "reject $Pattern"
}

try {
    # Real released executable, isolated directories; no provider binaries/state.
    $env:LOCALAPPDATA = Join-Path $testRoot 'user'
    [void][IO.Directory]::CreateDirectory($env:LOCALAPPDATA)
    $bundle = Join-Path $testRoot 'bundle'
    [void][IO.Directory]::CreateDirectory($bundle)
    $releaseRoot = 'https://github.com/ayushjainr/pikamux/releases'
    Invoke-WebRequest -UseBasicParsing "$releaseRoot/latest/download/pika-version" -OutFile (Join-Path $bundle 'pika-version')
    $version = [IO.File]::ReadAllText((Join-Path $bundle 'pika-version')).Trim()
    Check ($version -match '^\d+\.\d+\.\d+$') 'stable release selected'
    $name = "pikamux-$version-x86_64-pc-windows-msvc.zip"
    foreach ($file in @('pika-native-release.json', $name, "$name.sha256")) {
        Invoke-WebRequest -UseBasicParsing "$releaseRoot/download/v$version/$file" -OutFile (Join-Path $bundle $file)
    }
    & $installer -Bundle $bundle -NoPath
    $root = Join-Path $env:LOCALAPPDATA 'Pika\Client'
    $receiptPath = Join-Path $root 'install.json'
    $receiptBytes = [IO.File]::ReadAllText($receiptPath)
    $receipt = $receiptBytes | ConvertFrom-Json
    $directory = Join-Path $root "releases\$version-$($receipt.sha256.Substring(0,12))"
    Check ((& (Join-Path $directory 'pika.exe') --version) -ceq "pika $version") 'verified client installed'
    Check ($env:PATH -ceq $oldPath) 'NoPath preserves current PATH'
    Check ([Environment]::GetEnvironmentVariable('Path', 'User') -ceq $oldUserPath) 'NoPath preserves user PATH'
    & $installer -Bundle $bundle -NoPath
    Check (@(Get-ChildItem (Join-Path $root 'releases')).Count -eq 1) 'repeat install is idempotent'

    $bad = Join-Path $testRoot 'corrupt'
    Copy-Item -LiteralPath $bundle -Destination $bad -Recurse
    [IO.File]::AppendAllText((Join-Path $bad $name), 'corrupted')
    Reject { & $installer -Bundle $bad -NoPath } 'verification failed'
    Check ([IO.File]::ReadAllText($receiptPath) -ceq $receiptBytes) 'corruption preserves installed receipt'

    $bad = Join-Path $testRoot 'traversal'
    Copy-Item -LiteralPath $bundle -Destination $bad -Recurse
    $archivePath = Join-Path $bad $name
    $zip = [IO.Compression.ZipFile]::Open($archivePath, [IO.Compression.ZipArchiveMode]::Update)
    try {
        $zip.GetEntry('LICENSE').Delete()
        $entry = $zip.CreateEntry('../outside.txt')
        $writer = New-Object IO.StreamWriter($entry.Open())
        $writer.Write('not allowed')
        $writer.Dispose()
    } finally { $zip.Dispose() }
    $manifestPath = Join-Path $bad 'pika-native-release.json'
    $manifest = [IO.File]::ReadAllText($manifestPath) | ConvertFrom-Json
    $hash = (Get-FileHash $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
    $manifest.artifacts.'x86_64-pc-windows-msvc'.sha256 = $hash
    $manifest.artifacts.'x86_64-pc-windows-msvc'.bytes = (Get-Item $archivePath).Length
    [IO.File]::WriteAllText($manifestPath, ($manifest | ConvertTo-Json -Depth 10), $utf8)
    [IO.File]::WriteAllText((Join-Path $bad "$name.sha256"), "$hash  $name`n", $utf8)
    $originalLocal = $env:LOCALAPPDATA
    $env:LOCALAPPDATA = Join-Path $testRoot 'traversal-user'
    [void][IO.Directory]::CreateDirectory($env:LOCALAPPDATA)
    Reject { & $installer -Bundle $bad -NoPath } 'Unsafe ZIP entry'
    Check (-not (Test-Path (Join-Path $env:LOCALAPPDATA 'Pika\Client\outside.txt'))) 'ZIP traversal cannot write outside stage'
    $env:LOCALAPPDATA = $originalLocal
    Check ([IO.File]::ReadAllText($receiptPath) -ceq $receiptBytes) 'unsafe ZIP preserves installed receipt'

    [IO.File]::AppendAllText((Join-Path $directory 'LICENSE'), 'tampered')
    Reject { & $installer -Bundle $bundle -NoPath } 'Retained release bytes changed'

    $env:LOCALAPPDATA = Join-Path $testRoot 'foreign'
    $foreign = Join-Path $env:LOCALAPPDATA 'Pika\Client'
    [void][IO.Directory]::CreateDirectory($foreign)
    [IO.File]::WriteAllText((Join-Path $foreign 'keep.txt'), 'keep', $utf8)
    Reject { & $installer -Bundle $bundle -NoPath } 'another installation'
    Check ([IO.File]::ReadAllText((Join-Path $foreign 'keep.txt')) -ceq 'keep') 'foreign installation preserved'

    # Exercise the exact pasteable script shape and production HTTPS downloads.
    # The runner account is disposable; restore user PATH even if a check fails.
    $env:LOCALAPPDATA = Join-Path $testRoot 'online'
    [void][IO.Directory]::CreateDirectory($env:LOCALAPPDATA)
    Get-Content -LiteralPath $installer -Raw | Invoke-Expression
    $command = Get-Command pika -CommandType Application
    Check ($command.Source.StartsWith($env:LOCALAPPDATA)) 'copy-paste install makes pika available now'
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    Check ($userPath.Split(';')[0] -ceq (Split-Path $command.Source)) 'installer persists per-user PATH'
    & $installer
    Check (([Environment]::GetEnvironmentVariable('Path', 'User')) -ceq $userPath) 'repeat install does not duplicate PATH'
    Check (-not (Test-Path (Join-Path $env:LOCALAPPDATA 'Pika\client.json'))) 'installation does not pair a host'
} finally {
    [Environment]::SetEnvironmentVariable('Path', $oldUserPath, 'User')
    $env:PATH = $oldPath
    $env:LOCALAPPDATA = $oldLocal
    [IO.Directory]::Delete($testRoot, $true)
}
