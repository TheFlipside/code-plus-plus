<#
.SYNOPSIS
    Package a Windows release build into a zip.

.DESCRIPTION
    Reads the binaries `cargo build --release --target <Target>` left in
    `target\<Target>\release` and writes
    `codepp-<Label>-<version>.zip` into <OutDir>. The zip holds
    `codepp.exe`, the bundled plugin DLLs and the licence files, with no
    folder around them: Explorer's "Extract All" makes one named after
    the zip.

    The plugin DLLs sit beside the executable, which is where
    `codepp_platform::stage_bundled_plugins` looks for them on first
    start.

    The version comes from CODEPP_RELEASE_VERSION, which the workflow
    also gives the build, so the zip's name and the About dialog agree.

    Run from the repository root. Written for Windows PowerShell 5.1.

.EXAMPLE
    .forgejo\scripts\package-windows.ps1 -Target x86_64-pc-windows-msvc `
        -Label windows-x64 -Notices release-notices\THIRD_PARTY_CRATES.md `
        -OutDir dist-release
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Target,
    [Parameter(Mandatory = $true)][string]$Label,
    [Parameter(Mandatory = $true)][string]$Notices,
    [Parameter(Mandatory = $true)][string]$OutDir
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$version = $env:CODEPP_RELEASE_VERSION
if ([string]::IsNullOrEmpty($version)) {
    throw 'CODEPP_RELEASE_VERSION must name the version being packaged'
}

# The preinstalled plugins (DESIGN.md section 6.6). `example_hello` is the SDK
# sample and is left out of a release; staging skips a plugin that is
# not beside the executable.
$plugins = @('cppmimetools', 'cppconverter', 'cppexport')

$binDir = Join-Path (Join-Path 'target' $Target) 'release'
$name = "codepp-$Label-$version"

$binaries = @(Join-Path $binDir 'codepp.exe')
foreach ($plugin in $plugins) {
    $binaries += Join-Path $binDir "$plugin.dll"
}
foreach ($binary in $binaries) {
    if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) {
        throw "no release binary at $binary"
    }
}
if (-not (Test-Path -LiteralPath $Notices -PathType Leaf) -or (Get-Item -LiteralPath $Notices).Length -eq 0) {
    throw "the crate licence notices are missing or empty: $Notices"
}

# The C and C++ runtimes are linked statically (.cargo/config.toml), so
# that the app starts on a machine without the Visual C++
# Redistributable. A build that lost that setting, to a RUSTFLAGS in the
# environment for one, still builds and still runs here, where Visual
# Studio supplies the DLLs. So look: a PE file names each DLL it imports
# in plain ASCII.
$runtimeDlls = '(?i)(vcruntime\d+(_\d+)?|msvcp\d+(_\w+)?|msvcr\d+|concrt\d+|vcomp\d+|ucrtbase|api-ms-win-crt-[a-z0-9-]+)\.dll'
foreach ($binary in $binaries) {
    $text = [System.Text.Encoding]::ASCII.GetString([System.IO.File]::ReadAllBytes((Resolve-Path -LiteralPath $binary)))
    $found = [regex]::Matches($text, $runtimeDlls) | ForEach-Object { $_.Value.ToLowerInvariant() } | Sort-Object -Unique
    if ($found) {
        throw "$binary imports the C runtime dynamically ($($found -join ', ')); it must be linked statically (.cargo/config.toml)"
    }
}

# `codepp.exe` is linked for the GUI subsystem (crates/app/src/main.rs).
# Linked for the console subsystem it still builds and still runs, and a
# start from a terminal shows nothing wrong; started from Explorer it
# opens a console window beside the editor, as the first alpha did. So
# look: the optional header's Subsystem field is 2 for a GUI program and
# 3 for a console one.
$exe = [System.IO.File]::ReadAllBytes((Resolve-Path -LiteralPath $binaries[0]))
$pe = [System.BitConverter]::ToInt32($exe, 0x3C)
if ($pe -lt 0 -or $pe + 94 -gt $exe.Length -or [System.BitConverter]::ToUInt32($exe, $pe) -ne 0x4550) {
    throw "$($binaries[0]) is not a PE file"
}
# The PE signature (4 bytes) and the COFF header (20) come before the
# optional header, whose Subsystem is at offset 68.
$subsystem = [System.BitConverter]::ToUInt16($exe, $pe + 4 + 20 + 68)
if ($subsystem -ne 2) {
    throw "$($binaries[0]) is linked for subsystem $subsystem, not the GUI subsystem (2); started from Explorer it would open a console window (crates/app/src/main.rs)"
}

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("codepp-package-" + [System.Guid]::NewGuid().ToString('N'))
$stage = Join-Path $work $name
New-Item -ItemType Directory -Force -Path $stage | Out-Null
try {
    foreach ($binary in $binaries) {
        Copy-Item -LiteralPath $binary -Destination $stage
    }
    # `.txt`, so that a double click opens the licence in Notepad.
    Copy-Item -LiteralPath 'LICENSE' -Destination (Join-Path $stage 'LICENSE.txt')
    Copy-Item -LiteralPath 'THIRD_PARTY_LICENSES.md' -Destination $stage
    Copy-Item -LiteralPath $Notices -Destination (Join-Path $stage 'THIRD_PARTY_CRATES.md')

    New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
    $archive = Join-Path (Resolve-Path -LiteralPath $OutDir) "$name.zip"
    if (Test-Path -LiteralPath $archive) {
        Remove-Item -LiteralPath $archive
    }
    # .NET's own zip writer rather than Compress-Archive, which in
    # PowerShell 5.1 reads every file into memory and has written
    # backslashes into entry names.
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    [System.IO.Compression.ZipFile]::CreateFromDirectory(
        $stage, $archive, [System.IO.Compression.CompressionLevel]::Optimal, $false)
}
finally {
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host "packaged $archive"
Get-Item -LiteralPath $archive | Format-List Name, Length
