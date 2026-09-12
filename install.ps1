# Pika's per-user Windows client installer. Compatible with Windows PowerShell 5.1.
[CmdletBinding()]
param(
    [string]$Bundle,
    [switch]$NoPath
)

& {
    param($Bundle, $NoPath)
    $ErrorActionPreference = 'Stop'
    Set-StrictMode -Version 2
    if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT -or
        -not [Environment]::Is64BitOperatingSystem) {
        throw 'Pika client installation requires 64-bit Windows.'
    }
    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $releaseRoot = 'https://github.com/ayushjainr/pikamux/releases'
    $target = 'x86_64-pc-windows-msvc'
    $utf8 = New-Object System.Text.UTF8Encoding($false)

    function Assert-PlainPath([string]$Path) {
        $cursor = [IO.Path]::GetFullPath($Path)
        while ($cursor) {
            if (Test-Path -LiteralPath $cursor) {
                $item = Get-Item -LiteralPath $cursor -Force
                if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) {
                    throw "Installation path must not contain links or junctions: $cursor"
                }
            }
            $cursor = [IO.Path]::GetDirectoryName($cursor)
        }
    }

    function Receive-File([string]$Name, [string]$Url, [long]$Limit) {
        $destination = Join-Path $scratch $Name
        if ($Bundle) {
            $source = Join-Path $Bundle $Name
            Assert-PlainPath $source
            $info = Get-Item -LiteralPath $source
            if ($info.PSIsContainer -or $info.Length -le 0 -or $info.Length -gt $Limit) {
                throw "Invalid bundle file: $Name"
            }
            [IO.File]::Copy($source, $destination, $false)
        } else {
            $uri = [Uri]$Url
            $response = $null
            for ($redirect = 0; $redirect -le 5; $redirect++) {
                if ($uri.Scheme -ne 'https') { throw 'Downloads require HTTPS.' }
                $request = [Net.HttpWebRequest]::Create($uri)
                $request.AllowAutoRedirect = $false
                $request.Timeout = 30000
                $request.ReadWriteTimeout = 30000
                $request.UserAgent = 'Pika-Installer'
                $response = $request.GetResponse()
                if ([int]$response.StatusCode -ge 300 -and [int]$response.StatusCode -lt 400) {
                    $location = $response.Headers['Location']
                    $response.Close()
                    if (-not $location -or $redirect -eq 5) { throw 'Invalid download redirect.' }
                    $uri = New-Object Uri($uri, $location)
                } else { break }
            }
            $inputStream = $null
            $outputStream = $null
            try {
                if ($response.ContentLength -gt $Limit) { throw "Download too large: $Name" }
                $inputStream = $response.GetResponseStream()
                $outputStream = [IO.File]::Open($destination, [IO.FileMode]::CreateNew)
                $buffer = New-Object byte[] 65536
                $total = 0L
                $clock = [Diagnostics.Stopwatch]::StartNew()
                while (($count = $inputStream.Read($buffer, 0, $buffer.Length)) -gt 0) {
                    $total += $count
                    if ($total -gt $Limit -or $clock.Elapsed.TotalSeconds -gt 120) {
                        throw "Download exceeded its safety limit: $Name"
                    }
                    $outputStream.Write($buffer, 0, $count)
                }
                $outputStream.Flush($true)
                if ($total -eq 0) { throw "Empty download: $Name" }
            } finally {
                if ($outputStream) { $outputStream.Dispose() }
                if ($inputStream) { $inputStream.Dispose() }
                if ($response) { $response.Close() }
            }
        }
        if ((Get-Item -LiteralPath $destination).Length -gt $Limit) {
            throw "File exceeded its safety limit: $Name"
        }
        return $destination
    }

    function Assert-Version([string]$Executable, [string]$Version) {
        $start = New-Object Diagnostics.ProcessStartInfo
        $start.FileName = $Executable
        $start.Arguments = '--version'
        $start.UseShellExecute = $false
        $start.CreateNoWindow = $true
        $start.RedirectStandardOutput = $true
        $start.RedirectStandardError = $true
        $process = [Diagnostics.Process]::Start($start)
        try {
            $stdout = $process.StandardOutput.ReadToEndAsync()
            $stderr = $process.StandardError.ReadToEndAsync()
            if (-not $process.WaitForExit(10000)) {
                $process.Kill()
                throw 'The downloaded client did not respond. Nothing activated.'
            }
            if (-not $stdout.Wait(1000) -or -not $stderr.Wait(1000) -or
                $process.ExitCode -ne 0 -or $stdout.Result.Trim() -cne "pika $Version") {
                throw 'The downloaded client failed its version check. Nothing activated.'
            }
        } finally { $process.Dispose() }
    }

    $scratch = Join-Path ([IO.Path]::GetTempPath()) ('pika-install-' + [Guid]::NewGuid().ToString('N'))
    $stage = $null
    $lock = $null
    $oldTls = [Net.ServicePointManager]::SecurityProtocol
    try {
        [Net.ServicePointManager]::SecurityProtocol = $oldTls -bor [Net.SecurityProtocolType]::Tls12
        Assert-PlainPath $scratch
        [void][IO.Directory]::CreateDirectory($scratch)
        $versionFile = Receive-File 'pika-version' "$releaseRoot/latest/download/pika-version" 128
        $versionText = [IO.File]::ReadAllText($versionFile)
        if ($versionText -cnotmatch '\A[0-9]+\.[0-9]+\.[0-9]+\r?\n\z') {
            throw 'Invalid stable release version.'
        }
        $version = $versionText.Trim()
        $base = "$releaseRoot/download/v$version"
        $manifestFile = Receive-File 'pika-native-release.json' "$base/pika-native-release.json" 65536
        $manifest = [IO.File]::ReadAllText($manifestFile) | ConvertFrom-Json
        $artifact = $manifest.artifacts.$target
        $archiveName = "pikamux-$version-$target.zip"
        if ($manifest.schema -ne 2 -or $manifest.package -cne 'pikamux' -or
            $manifest.channel -cne 'stable' -or $manifest.version -cne $version -or
            $artifact.file -cne $archiveName -or $artifact.sha256 -cnotmatch '\A[0-9a-f]{64}\z' -or
            $artifact.bytes -le 0 -or $artifact.bytes -gt 20971520) {
            throw 'Invalid Windows release manifest.'
        }
        $archive = Receive-File $archiveName "$base/$archiveName" 20971520
        $sidecar = Receive-File "$archiveName.sha256" "$base/$archiveName.sha256" 256
        $expectedSidecar = $artifact.sha256
        if ([IO.File]::ReadAllText($sidecar).Trim() -cne $expectedSidecar -or
            (Get-Item -LiteralPath $archive).Length -ne $artifact.bytes -or
            (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant() -cne $artifact.sha256) {
            throw 'Download verification failed. Nothing activated.'
        }

        $root = Join-Path $env:LOCALAPPDATA 'Pika\Client'
        Assert-PlainPath $root
        [void][IO.Directory]::CreateDirectory($root)
        $marker = Join-Path $root '.pika-client-install'
        if (-not (Test-Path -LiteralPath $marker)) {
            if (@(Get-ChildItem -LiteralPath $root -Force).Count -ne 0) {
                throw 'The client directory belongs to another installation. Nothing overwritten.'
            }
            [IO.File]::WriteAllText($marker, 'pika-windows-installer-v1', $utf8)
        }
        Assert-PlainPath $marker
        if ([IO.File]::ReadAllText($marker) -cne 'pika-windows-installer-v1') {
            throw 'Unrecognized client installation. Nothing overwritten.'
        }
        $lockPath = Join-Path $root '.install.lock'
        Assert-PlainPath $lockPath
        $lock = [IO.File]::Open($lockPath, [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
        $receiptPath = Join-Path $root 'install.json'
        Assert-PlainPath $receiptPath
        $previous = $null
        if (Test-Path -LiteralPath $receiptPath) {
            $previous = [IO.File]::ReadAllText($receiptPath) | ConvertFrom-Json
            if ($previous.schema -ne 1 -or $previous.package -cne 'pikamux' -or
                $previous.version -cnotmatch '\A[0-9]+\.[0-9]+\.[0-9]+\z' -or
                $previous.sha256 -cnotmatch '\A[0-9a-f]{64}\z') { throw 'Invalid installation receipt.' }
            if ([version]$version -lt [version]$previous.version -or
                ($version -ceq $previous.version -and $artifact.sha256 -cne $previous.sha256)) {
                throw 'Refusing a downgrade or changed bytes for an installed version.'
            }
        }
        $releases = Join-Path $root 'releases'
        Assert-PlainPath $releases
        [void][IO.Directory]::CreateDirectory($releases)
        $destination = Join-Path $releases "$version-$($artifact.sha256.Substring(0, 12))"
        Assert-PlainPath $destination
        $stage = Join-Path $root ('.stage-' + [Guid]::NewGuid().ToString('N'))
        [void][IO.Directory]::CreateDirectory($stage)
        $zip = [IO.Compression.ZipFile]::OpenRead($archive)
        try {
            $allowed = @('pika.exe', 'LICENSE', 'THIRD_PARTY.md')
            $seen = New-Object 'Collections.Generic.HashSet[string]' ([StringComparer]::Ordinal)
            if ($zip.Entries.Count -ne 3) { throw 'Unexpected ZIP contents.' }
            foreach ($entry in $zip.Entries) {
                if (-not ($allowed -ccontains $entry.FullName) -or -not $seen.Add($entry.FullName) -or
                    $entry.Length -le 0 -or $entry.Length -gt 52428800 -or
                    (($entry.ExternalAttributes -shr 16) -band 0xF000) -eq 0xA000 -or
                    ($entry.ExternalAttributes -band 0x400)) { throw 'Unsafe ZIP entry.' }
                $stream = $entry.Open()
                $output = [IO.File]::Open((Join-Path $stage $entry.FullName), [IO.FileMode]::CreateNew)
                try {
                    $buffer = New-Object byte[] 65536
                    $total = 0L
                    while (($count = $stream.Read($buffer, 0, $buffer.Length)) -gt 0) {
                        $total += $count
                        if ($total -gt $entry.Length) { throw 'ZIP entry exceeded its declared size.' }
                        $output.Write($buffer, 0, $count)
                    }
                    if ($total -ne $entry.Length) { throw 'Truncated ZIP entry.' }
                    $output.Flush($true)
                } finally { $output.Dispose(); $stream.Dispose() }
            }
        } finally { $zip.Dispose() }
        Assert-Version (Join-Path $stage 'pika.exe') $version
        if (Test-Path -LiteralPath $destination) {
            foreach ($name in @('pika.exe', 'LICENSE', 'THIRD_PARTY.md')) {
                $existing = Join-Path $destination $name
                Assert-PlainPath $existing
                if ((Get-FileHash -LiteralPath $existing).Hash -cne (Get-FileHash -LiteralPath (Join-Path $stage $name)).Hash) {
                    throw 'Retained release bytes changed. Nothing activated.'
                }
            }
        } else { [IO.Directory]::Move($stage, $destination); $stage = $null }

        $receipt = @{ schema = 1; package = 'pikamux'; version = $version; sha256 = $artifact.sha256 }
        $pendingReceipt = Join-Path $root ('.receipt-' + [Guid]::NewGuid().ToString('N'))
        [IO.File]::WriteAllText($pendingReceipt, ($receipt | ConvertTo-Json), $utf8)
        if (Test-Path -LiteralPath $receiptPath) {
            [IO.File]::Replace($pendingReceipt, $receiptPath, $null)
        } else { [IO.File]::Move($pendingReceipt, $receiptPath) }
        if (-not $NoPath) {
            $oldDirectory = $null
            if ($previous) { $oldDirectory = Join-Path $releases "$($previous.version)-$($previous.sha256.Substring(0, 12))" }
            $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
            $parts = @($userPath -split ';' | Where-Object { $_ -and $_ -ine $destination -and $_ -ine $oldDirectory })
            [Environment]::SetEnvironmentVariable('Path', ((@($destination) + $parts) -join ';'), 'User')
            $parts = @($env:Path -split ';' | Where-Object { $_ -and $_ -ine $destination -and $_ -ine $oldDirectory })
            $env:Path = (@($destination) + $parts) -join ';'
        }
        Write-Host "Pika installed - $destination\pika.exe"
        Write-Host 'Windows client for agents hosted on macOS or Linux.'
        Write-Host 'Next: pika setup YOUR_SSH_HOST'
        Write-Host 'Use the same SSH host name you already connect to. Pairing remains experimental.'
    } finally {
        if ($lock) { $lock.Dispose() }
        if ($stage -and (Test-Path -LiteralPath $stage)) { [IO.Directory]::Delete($stage, $true) }
        if (Test-Path -LiteralPath $scratch) { [IO.Directory]::Delete($scratch, $true) }
        [Net.ServicePointManager]::SecurityProtocol = $oldTls
    }
} $Bundle $NoPath
