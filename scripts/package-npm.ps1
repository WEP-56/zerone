[CmdletBinding()]
param(
    [string]$PackageName = "zerone-agent",
    [string]$Version = "",
    [string]$ArtifactsDir = "",
    [string]$OutputDir = "",
    [switch]$Pack,
    [switch]$RequireAllPlatforms
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$isWindowsPlatform = [Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
    [Runtime.InteropServices.OSPlatform]::Windows
)
$isLinuxPlatform = [Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
    [Runtime.InteropServices.OSPlatform]::Linux
)
$isMacOSPlatform = [Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
    [Runtime.InteropServices.OSPlatform]::OSX
)

$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot ".."))
if (-not $OutputDir) {
    $OutputDir = Join-Path $repoRoot "dist/npm"
}
$outputRoot = [IO.Path]::GetFullPath($OutputDir)
$repoPrefix = $repoRoot.TrimEnd([IO.Path]::DirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
if (-not $outputRoot.StartsWith($repoPrefix, [StringComparison]::OrdinalIgnoreCase)) {
    throw "OutputDir must stay inside the repository: $outputRoot"
}

$cargoToml = Get-Content -Raw -Encoding utf8 (Join-Path $repoRoot "Cargo.toml")
if (-not $Version) {
    $versionMatch = [regex]::Match($cargoToml, '(?m)^version\s*=\s*"([^"]+)"')
    if (-not $versionMatch.Success) {
        throw "Could not read package.version from Cargo.toml"
    }
    $Version = $versionMatch.Groups[1].Value
}
if ($PackageName -notmatch '^(?:@[a-z0-9._-]+/)?[a-z0-9._-]+$') {
    throw "Invalid npm package name: $PackageName"
}
if ($Version -notmatch '^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$') {
    throw "Version must be a semver value: $Version"
}

if (Test-Path -LiteralPath $outputRoot) {
    Remove-Item -LiteralPath $outputRoot -Recurse -Force
}
$packageDir = Join-Path $outputRoot "package"
$vendorDir = Join-Path $packageDir "vendor"
New-Item -ItemType Directory -Force -Path (Join-Path $packageDir "bin"), $vendorDir | Out-Null

Copy-Item -LiteralPath (Join-Path $repoRoot "npm/bin/zerone.js") -Destination (Join-Path $packageDir "bin/zerone.js")
Copy-Item -LiteralPath (Join-Path $repoRoot "readme.md") -Destination (Join-Path $packageDir "README.md")

$platforms = [ordered]@{
    "win32-x64" = "zerone.exe"
    "linux-x64" = "zerone"
    "darwin-x64" = "zerone"
    "darwin-arm64" = "zerone"
}
$included = [Collections.Generic.List[string]]::new()

function Add-PlatformBinary([string]$Platform, [string]$Source) {
    $destination = Join-Path $vendorDir $Platform
    New-Item -ItemType Directory -Force -Path $destination | Out-Null
    Copy-Item -LiteralPath $Source -Destination (Join-Path $destination $platforms[$Platform])
    [void]$included.Add($Platform)
}

if ($ArtifactsDir) {
    $artifactsRoot = [IO.Path]::GetFullPath($ArtifactsDir)
    foreach ($platform in $platforms.Keys) {
        $source = Join-Path (Join-Path $artifactsRoot $platform) $platforms[$platform]
        if (Test-Path -LiteralPath $source) {
            Add-PlatformBinary $platform $source
        }
    }
} else {
    $architecture = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString().ToLowerInvariant()
    if ($architecture -eq "x64") { $architecture = "x64" }
    elseif ($architecture -eq "arm64") { $architecture = "arm64" }
    else { throw "Unsupported local architecture: $architecture" }

    if ($isWindowsPlatform) { $platform = "win32-$architecture"; $binaryName = "zerone.exe" }
    elseif ($isLinuxPlatform) { $platform = "linux-$architecture"; $binaryName = "zerone" }
    elseif ($isMacOSPlatform) { $platform = "darwin-$architecture"; $binaryName = "zerone" }
    else { throw "Unsupported local operating system" }
    if (-not $platforms.Contains($platform)) {
        throw "The npm launcher does not currently support $platform"
    }

    $targetDir = Join-Path $repoRoot "target/npm-build"
    & cargo build --release --locked --target-dir $targetDir
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
    Add-PlatformBinary $platform (Join-Path (Join-Path $targetDir "release") $binaryName)
}

$missing = @($platforms.Keys | Where-Object { -not $included.Contains($_) })
if ($RequireAllPlatforms -and $missing.Count -gt 0) {
    throw "Missing platform artifacts: $($missing -join ', ')"
}

$packageJson = [ordered]@{
    name = $PackageName
    version = $Version
    description = "A minimal but structurally complete coding agent"
    type = "module"
    bin = [ordered]@{ zerone = "bin/zerone.js" }
    files = @("bin", "vendor", "README.md")
    engines = [ordered]@{ node = ">=18" }
    repository = [ordered]@{
        type = "git"
        url = "git+https://github.com/WEP-56/zerone.git"
    }
    bugs = [ordered]@{ url = "https://github.com/WEP-56/zerone/issues" }
    homepage = "https://github.com/WEP-56/zerone#readme"
    keywords = @("agent", "coding-agent", "tui", "rust")
}
$json = $packageJson | ConvertTo-Json -Depth 8
$utf8NoBom = New-Object Text.UTF8Encoding($false)
[IO.File]::WriteAllText((Join-Path $packageDir "package.json"), $json, $utf8NoBom)

if (-not $isWindowsPlatform) {
    & chmod +x (Join-Path $packageDir "bin/zerone.js")
    foreach ($platform in $included) {
        if ($platform -notlike "win32-*") {
            & chmod +x (Join-Path (Join-Path $vendorDir $platform) "zerone")
        }
    }
}

Write-Host "Generated npm package: $packageDir"
Write-Host "Included platforms: $($included -join ', ')"
if ($missing.Count -gt 0) {
    Write-Warning "This local package is missing: $($missing -join ', '). Use the GitHub Actions artifact for a public cross-platform release."
}

if ($Pack) {
    & npm pack $packageDir --pack-destination $outputRoot
    if ($LASTEXITCODE -ne 0) { throw "npm pack failed" }
    $tarball = Get-ChildItem -LiteralPath $outputRoot -Filter "*.tgz" | Sort-Object LastWriteTime -Descending | Select-Object -First 1
    Write-Host "Created tarball: $($tarball.FullName)"
    Write-Host "Publish: npm publish $($tarball.FullName) --access public"
}
