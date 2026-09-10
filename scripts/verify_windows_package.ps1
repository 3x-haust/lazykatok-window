#Requires -Version 7.0
param(
    [Parameter(Mandatory = $true)][string]$PackageDirectory,
    [string]$Fixture = "$PSScriptRoot/../tests/fixtures/kakao/replies.jsonl"
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# Exercise only the checked-in synthetic fixture, from a path with spaces and Korean.
# No KakaoTalk process or installed account is accessed by this check.
$scratch = Join-Path ([IO.Path]::GetTempPath()) ("lazykatok 한글 package " + [guid]::NewGuid())
$originalPath = $env:PATH
$originalEmbedder = $env:KATOK_EMBEDDER
$package = (Resolve-Path $PackageDirectory).Path
if (-not (Test-Path (Join-Path $package 'DirectML.dll') -PathType Leaf)) {
    throw 'The pinned Windows inference runtime requires its bundled DirectML.dll'
}
$fixturePath = (Resolve-Path $Fixture).Path
New-Item -ItemType Directory -Path $scratch | Out-Null
try {
    Copy-Item -Path $package -Destination (Join-Path $scratch 'app') -Recurse
    $localFixture = Join-Path $scratch '합성 입력.jsonl'
    Copy-Item -LiteralPath $fixturePath -Destination $localFixture
    $executable = Join-Path $scratch 'app/lazykatok.exe'
    $data = Join-Path $scratch 'private data'
    $env:PATH = "$env:SystemRoot\System32;$env:SystemRoot"
    $env:KATOK_EMBEDDER = 'local-test'
    function Invoke-CheckedJson([string[]]$Arguments) {
        $output = & $executable --data-dir $data @Arguments
        if ($LASTEXITCODE -ne 0) { throw "Packaged command failed: $($Arguments[0]) ($LASTEXITCODE)" }
        return ($output | Out-String | ConvertFrom-Json)
    }
    & $executable --version
    if ($LASTEXITCODE -ne 0) { throw 'Packaged executable did not start' }
    $first = Invoke-CheckedJson -Arguments @('sync', '--source', 'fixture', $localFixture, '--json')
    if ($first.inserted_messages -ne 3) { throw 'Synthetic fixture import did not insert 3 messages' }
    $second = Invoke-CheckedJson -Arguments @('sync', '--source', 'fixture', $localFixture, '--json')
    if ($second.inserted_messages -ne 0) { throw 'Synthetic import was not idempotent' }
    $search = Invoke-CheckedJson -Arguments @('search', 'keyword', '보고서', '--json')
    if (($search | ConvertTo-Json -Depth 20) -notmatch 'chunk_2aeac4db0a04ceb2') {
        throw 'Packaged Unicode keyword search failed'
    }
    Write-Output 'PASS: packaged executable, Unicode paths, private data creation, fixture sync, repeat sync, keyword search'
} finally {
    $env:PATH = $originalPath
    $env:KATOK_EMBEDDER = $originalEmbedder
    Remove-Item -LiteralPath $scratch -Recurse -Force
}
