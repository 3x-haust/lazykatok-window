#Requires -Version 7.0
param(
    [string]$BuildDirectory = 'target/release',
    [string]$OutputDirectory = 'dist/lazykatok-windows-x64'
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$repoRoot = Split-Path $PSScriptRoot -Parent
$BuildDirectory = [IO.Path]::GetFullPath($BuildDirectory, $repoRoot)
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory, $repoRoot)
if (-not $env:VCToolsRedistDir) {
    throw 'Run in the x64 Native Tools environment so VCToolsRedistDir identifies the redistributable runtime.'
}
$crt = @(Get-ChildItem (Join-Path $env:VCToolsRedistDir 'x64') -Directory -Filter 'Microsoft.VC*.CRT')
if ($crt.Count -ne 1) { throw 'Expected one x64 Microsoft C++ redistributable directory' }
if (Test-Path $OutputDirectory) { throw 'Output directory already exists; choose a fresh package directory' }
$documents = @('README.md','LICENSE','THIRD_PARTY_NOTICES.md','ACCEPTABLE_USE_POLICY.md','DISCLAIMER.md','docs/windows.md') |
    ForEach-Object { (Resolve-Path (Join-Path $repoRoot $_)).Path }
$executable = (Resolve-Path (Join-Path $BuildDirectory 'lazykatok.exe')).Path
New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
Copy-Item $executable $OutputDirectory
Get-ChildItem $BuildDirectory -Filter '*.dll' -File | Copy-Item -Destination $OutputDirectory
# The CI runner has Visual Studio installed. Bundle its app-local redistributables
# so a fresh PC does not silently depend on that development environment.
Get-ChildItem $crt[0].FullName -Filter '*.dll' -File | Copy-Item -Destination $OutputDirectory
Copy-Item $documents $OutputDirectory
$hashes = Get-ChildItem $OutputDirectory -File | Sort-Object Name | ForEach-Object {
    $hash = (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
    "$hash  $($_.Name)"
}
$hashes | Set-Content (Join-Path $OutputDirectory 'SHA256SUMS.txt') -Encoding utf8NoBOM
