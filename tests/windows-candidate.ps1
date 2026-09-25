# Pure installer decision tests. Runs on PowerShell 7 anywhere and Windows 5.1.
# Synthetic PE bytes and mocked trust results; no signing store or policy writes.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2
$installer = Join-Path (Split-Path $PSScriptRoot -Parent) 'install.ps1'
$tokens = $null
$errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile($installer, [ref]$tokens, [ref]$errors)
if ($errors.Count) { throw ($errors | Out-String) }
$candidate = $ast.Find({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Assert-Candidate' }, $true)
if (-not $candidate) { throw 'Missing candidate verifier' }
Invoke-Expression $candidate.Extent.Text

function Check([bool]$Pass, [string]$Message) {
    if (-not $Pass) { throw "FAILED: $Message" }
    Write-Host "PASS: $Message"
}
function Reject([scriptblock]$Action, [string]$Pattern) {
    $message = $null
    try { & $Action } catch { $message = $_.ToString() }
    Check ($null -ne $message -and $message -match $Pattern) "reject $Pattern"
}
function Get-AuthenticodeSignature([string]$LiteralPath) {
    $script:trustCalls++
    return $script:trust
}
function Assert-Version([string]$Executable, [string]$Version) {
    $script:launchCalls++
    Check ($Version -ceq '1.2.3') 'legacy probe receives pinned version'
    if ($script:denyLaunch) { throw 'Windows blocked legacy launch' }
}
$root = Join-Path ([IO.Path]::GetTempPath()) ('pika-candidate-test-' + [Guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory($root)
try {
    $exe = Join-Path $root 'pika.exe'
    $bytes = New-Object byte[] 65536
    $bytes[0] = 0x4D; $bytes[1] = 0x5A; $bytes[0x3C] = 0x80
    $bytes[0x80] = 0x50; $bytes[0x81] = 0x45
    $bytes[0x84] = 0x64; $bytes[0x85] = 0x86
    $bytes[0x98] = 0x0B; $bytes[0x99] = 0x02
    [IO.File]::WriteAllBytes($exe, $bytes)
    $script:trustCalls = 0; $script:launchCalls = 0; $script:denyLaunch = $false
    $script:trust = [pscustomobject]@{ Status = 'Valid'; SignerCertificate = 'fixture'; TimeStamperCertificate = 'fixture' }
    Assert-Candidate $exe '1.2.3'
    Check ($script:trustCalls -eq 1 -and $script:launchCalls -eq 0) 'trusted timestamped release is not executed in staging'
    foreach ($status in @('HashMismatch', 'NotTrusted', 'UnknownError', 'NotSupportedFileFormat')) {
        $script:trust.Status = $status
        Reject { Assert-Candidate $exe '1.2.3' } 'signature or timestamp validation failed'
    }
    $script:trust.Status = 'Valid'; $script:trust.TimeStamperCertificate = $null
    Reject { Assert-Candidate $exe '1.2.3' } 'signature or timestamp validation failed'
    $script:trust.TimeStamperCertificate = 'fixture'; $script:trust.SignerCertificate = $null
    Reject { Assert-Candidate $exe '1.2.3' } 'signature or timestamp validation failed'
    Check ($script:launchCalls -eq 0) 'bad signatures never fall back to execution'
    $script:trust.Status = 'NotSigned'
    Assert-Candidate $exe '1.2.3'
    Check ($script:launchCalls -eq 1) 'legacy unsigned release retains startup check'
    $script:denyLaunch = $true
    Reject { Assert-Candidate $exe '1.2.3' } 'Windows blocked legacy launch'
    $before = $script:trustCalls
    $bytes[0x84] = 0x4C
    [IO.File]::WriteAllBytes($exe, $bytes)
    Reject { Assert-Candidate $exe '1.2.3' } 'Expected an x64'
    $bytes[0x84] = 0x64; $bytes[0x99] = 0x01
    [IO.File]::WriteAllBytes($exe, $bytes)
    Reject { Assert-Candidate $exe '1.2.3' } 'Expected an x64'
    $bytes[0x3F] = 0xFF
    [IO.File]::WriteAllBytes($exe, $bytes)
    Reject { Assert-Candidate $exe '1.2.3' } 'Expected an x64'
    [IO.File]::WriteAllBytes($exe, [byte[]]@(0x4D, 0x5A))
    Reject { Assert-Candidate $exe '1.2.3' } 'Invalid Windows executable'
    Check ($script:trustCalls -eq $before) 'malformed PE rejected before trust lookup'
} finally {
    [IO.Directory]::Delete($root, $true)
}
