<#
.SYNOPSIS
    Package a Windows release build into a zip and an installer.

.DESCRIPTION
    Reads the binaries `cargo build --release --target <Target>` left in
    `target\<Target>\release` and writes two files into <OutDir>, both
    made from one folder of files:

      codepp-<Label>-<version>.zip  `codepp.exe`, the bundled plugin
                                    DLLs and the licence files, with no
                                    folder around them: Explorer's
                                    "Extract All" makes one named after
                                    the zip.
      codepp-<Label>-<version>.msi  an installer for the same files and
                                    one more, `WinSparkle.dll`, built
                                    from packaging\windows\codepp.wxs,
                                    which says what else it sets up.

    The plugin DLLs sit beside the executable in both, which is where
    `codepp_platform::stage_bundled_plugins` looks for them on first
    start.

    `WinSparkle.dll` is the updater's library (DESIGN.md section 11).
    Only an installed Code++ updates itself, so only the installer
    carries it; a Code++ run from the zip would never load it. It is
    not built here: the script fetches WinSparkle's release zip from
    GitHub at each run, whose version and SHA-256 are written below,
    and takes the one file out of it.

    The version comes from CODEPP_RELEASE_VERSION, which the workflow
    also gives the build, so the files' names and the About dialog
    agree.

    The installer is built with WiX, which runs on .NET: `dotnet` must
    be on PATH. The script fetches WiX and its UI extension from
    nuget.org at each run, as the two package files whose version and
    SHA-256 are written below, and runs them from where it unpacked
    them. Nothing of WiX is installed, and no copy the machine may
    have cached is used.

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
# The workflow has checked the version already (release-version.sh).
# Checked again, because it goes into file names, a command line and
# the installer's tables, and this script can be run by hand.
if ($version -cnotmatch '\A[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?\z') {
    throw "CODEPP_RELEASE_VERSION is not x.y.z or x.y.z-<pre-release> (got '$version')"
}

# WiX and its UI extension, as nuget.org publishes them: the version,
# and the SHA-256 of each package file. A package is used only if the
# file that arrives has this hash, so the installer is built by exactly
# these bytes on every runner and at every run. To move to another
# version, download the two files from the URL in Get-VerifiedPackage,
# compare their SHA-512 with `packageHash` in nuget.org's catalogue
# entry for the version, and write the version and the new SHA-256
# here; then build the installer and install it once by hand.
$wixVersion = '5.0.2'
$wixPackages = @{
    'wix'                  = 'F30EF0C74E2A986126539C5780BE93AC24E8136EAF723B1937B26272703AE173'
    'wixtoolset.ui.wixext' = '5EF2C707614B9F70B6BBADD2D4ABCB4124EFEE215E9B16BFBC80113079A604C7'
}

# WinSparkle, as its project publishes it on GitHub: the version, the
# SHA-256 of the release zip, which is the digest GitHub shows for the
# file, and the SHA-256 of the one file taken out of it, the x64 DLL.
# The second says which bytes an installation holds, to whoever wants
# to compare. Code++ refuses a WinSparkle.dll of any other version
# (crates\platform\src\winsparkle.rs), so the version here and
# `EXPECTED_VERSION` there move together; so do the notices in
# THIRD_PARTY_LICENSES.md, which name what this version bundles. To
# move to another version, download the zip, compare its SHA-256 with
# the digest on the release's page, write both hashes here, run the
# binding's opt-in test against the new DLL (docs/DEVELOPMENT.md),
# then build the installer and update an installed Code++ with it once.
$winSparkleVersion = '0.9.4'
$winSparkleZipSha256 = '6037DF37FC263BD1650A1C4949681A9D40FFE991D01F35892A406CB5D103C976'
$winSparkleDllSha256 = '9B43B1C16EE39FB9A91B5BD75138767898779510E0836BE2919250607CDBE8AB'
# The release zip holds a DLL for each architecture; this is the one
# for the only target a Windows release is built for.
if ($Target -ne 'x86_64-pc-windows-msvc') {
    throw "no WinSparkle.dll is pinned for the target '$Target', only for x86_64-pc-windows-msvc"
}

# The preinstalled plugins (DESIGN.md section 6.6). `example_hello` is the SDK
# sample and is left out of a release; staging skips a plugin that is
# not beside the executable.
$plugins = @('cppmimetools', 'cppconverter', 'cppexport')

# Run a program and stop on a non-zero exit, which PowerShell on its own
# lets pass.
function Invoke-Tool {
    param(
        [Parameter(Mandatory = $true)][string]$What,
        [Parameter(Mandatory = $true)][string]$Exe,
        [Parameter(Mandatory = $true)][string[]]$Arguments
    )
    & $Exe @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$What failed with exit code $LASTEXITCODE"
    }
}

# The licence as the RTF the installer's licence page shows. A paragraph
# of the text file becomes one paragraph, so that the page wraps it to
# its own width instead of showing the file's line breaks.
function ConvertTo-LicenceRtf {
    param([Parameter(Mandatory = $true)][string]$Text)
    $rtf = New-Object System.Text.StringBuilder
    [void]$rtf.Append('{\rtf1\ansi\deff0{\fonttbl{\f0\fnil Segoe UI;}}\pard\f0\fs18 ')
    $paragraphs = ($Text -replace "`r`n", "`n").Trim() -split "`n[ `t]*`n"
    foreach ($paragraph in $paragraphs) {
        $line = ($paragraph -split "`n" | ForEach-Object { $_.Trim() }) -join ' '
        foreach ($char in $line.ToCharArray()) {
            $code = [int]$char
            if ($char -eq '\' -or $char -eq '{' -or $char -eq '}') {
                [void]$rtf.Append('\').Append($char)
            }
            elseif ($code -ge 32 -and $code -lt 127) {
                [void]$rtf.Append($char)
            }
            else {
                # RTF takes a character outside ASCII as a signed 16-bit
                # number, followed by what a reader without Unicode shows.
                if ($code -gt 32767) { $code -= 65536 }
                [void]$rtf.Append('\u').Append($code).Append('?')
            }
        }
        [void]$rtf.Append('\par\par ')
    }
    [void]$rtf.Append('}')
    $rtf.ToString()
}

# Download <Url> to <File> and refuse it unless it has the expected
# SHA-256.
function Get-VerifiedFile {
    param(
        [Parameter(Mandatory = $true)][string]$Url,
        [Parameter(Mandatory = $true)][string]$Sha256,
        [Parameter(Mandatory = $true)][string]$File
    )
    # Windows PowerShell 5.1 does not offer TLS 1.2 unless asked.
    [System.Net.ServicePointManager]::SecurityProtocol =
        [System.Net.ServicePointManager]::SecurityProtocol -bor [System.Net.SecurityProtocolType]::Tls12
    # With a time limit, so that a connection that stalls fails the job
    # instead of holding it; and without the progress display, which in
    # Windows PowerShell 5.1 slows a download many times over.
    $ProgressPreference = 'SilentlyContinue'
    try {
        Invoke-WebRequest -Uri $Url -OutFile $File -UseBasicParsing -TimeoutSec 120
    }
    catch {
        throw "could not download ${Url}: $($_.Exception.Message)"
    }
    $found = (Get-FileHash -LiteralPath $File -Algorithm SHA256).Hash
    if ($found -ne $Sha256) {
        throw "$Url has SHA-256 $found, not the $Sha256 this script expects; it is not used"
    }
}

# Fetch one package from nuget.org into <Into>, refuse it unless it has
# the expected SHA-256, and unpack it. Returns the folder it is in.
function Get-VerifiedPackage {
    param(
        [Parameter(Mandatory = $true)][string]$Id,
        [Parameter(Mandatory = $true)][string]$Version,
        [Parameter(Mandatory = $true)][string]$Sha256,
        [Parameter(Mandatory = $true)][string]$Into
    )
    $file = Join-Path $Into "$Id.$Version.nupkg"
    Get-VerifiedFile "https://api.nuget.org/v3-flatcontainer/$Id/$Version/$Id.$Version.nupkg" $Sha256 $file
    $folder = Join-Path $Into $Id
    [System.IO.Compression.ZipFile]::ExtractToDirectory($file, $folder)
    $folder
}

# Fetch WinSparkle's release zip into <Into>, refuse it unless it has
# the expected SHA-256, and take the x64 DLL out of it into <To>, which
# then holds that one file. Nothing else of the zip is unpacked.
function Get-WinSparkleDll {
    param(
        [Parameter(Mandatory = $true)][string]$Version,
        [Parameter(Mandatory = $true)][string]$ZipSha256,
        [Parameter(Mandatory = $true)][string]$DllSha256,
        [Parameter(Mandatory = $true)][string]$Into,
        [Parameter(Mandatory = $true)][string]$To
    )
    $zip = Join-Path $Into "WinSparkle-$Version.zip"
    Get-VerifiedFile "https://github.com/vslavik/winsparkle/releases/download/v$Version/WinSparkle-$Version.zip" $ZipSha256 $zip
    $entryName = "WinSparkle-$Version/x64/Release/WinSparkle.dll"
    New-Item -ItemType Directory -Force -Path $To | Out-Null
    $dll = Join-Path $To 'WinSparkle.dll'
    $archive = [System.IO.Compression.ZipFile]::OpenRead($zip)
    try {
        $entries = @($archive.Entries | Where-Object { $_.FullName -ceq $entryName })
        if ($entries.Count -ne 1) {
            throw "expected one $entryName in WinSparkle's release zip, found $($entries.Count)"
        }
        [System.IO.Compression.ZipFileExtensions]::ExtractToFile($entries[0], $dll)
    }
    finally {
        $archive.Dispose()
    }
    $found = (Get-FileHash -LiteralPath $dll -Algorithm SHA256).Hash
    if ($found -ne $DllSha256) {
        throw "$entryName has SHA-256 $found, not the $DllSha256 this script expects; it is not used"
    }
    $dll
}

# The one file of that name below a folder.
function Get-OnlyFile {
    param(
        [Parameter(Mandatory = $true)][string]$Folder,
        [Parameter(Mandatory = $true)][string]$Name
    )
    $found = @(Get-ChildItem -LiteralPath $Folder -Recurse -File -Filter $Name)
    if ($found.Count -ne 1) {
        throw "expected one $Name below $Folder, found $($found.Count)"
    }
    $found[0].FullName
}

# The rows a query selects from an installer's tables, read through
# Windows Installer's own automation interface: one string a row, its
# first <Columns> columns joined by tabs. Like any PowerShell function
# it hands back nothing, one string or an array, so every caller wraps
# the call in @( ).
function Get-MsiRows {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Query,
        [int]$Columns = 1
    )
    $invoke = [System.Reflection.BindingFlags]::InvokeMethod
    $get = [System.Reflection.BindingFlags]::GetProperty
    $installer = $null
    $database = $null
    $view = $null
    try {
        $installer = New-Object -ComObject WindowsInstaller.Installer
        # 0: read-only.
        $database = $installer.GetType().InvokeMember('OpenDatabase', $invoke, $null, $installer, @($Path, 0))
        $view = $database.GetType().InvokeMember('OpenView', $invoke, $null, $database, @($Query))
        [void]$view.GetType().InvokeMember('Execute', $invoke, $null, $view, $null)
        $values = @()
        while ($true) {
            $record = $view.GetType().InvokeMember('Fetch', $invoke, $null, $view, $null)
            if ($null -eq $record) { break }
            $cells = for ($column = 1; $column -le $Columns; $column++) {
                [string]$record.GetType().InvokeMember('StringData', $get, $null, $record, @([int]$column))
            }
            $values += ($cells -join "`t")
            [void][System.Runtime.InteropServices.Marshal]::ReleaseComObject($record)
        }
        $values
    }
    finally {
        # Released by hand, whichever step failed: the database object
        # keeps the file open until it goes.
        if ($null -ne $view) {
            [void]$view.GetType().InvokeMember('Close', $invoke, $null, $view, $null)
        }
        foreach ($object in @($view, $database, $installer)) {
            if ($null -ne $object) {
                [void][System.Runtime.InteropServices.Marshal]::ReleaseComObject($object)
            }
        }
    }
}

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

# What the installer is built from and with. Checked here, so that a
# runner without .NET fails at once and says why.
$wxs = Join-Path (Join-Path 'packaging' 'windows') 'codepp.wxs'
$icon = Join-Path 'assets' 'code++.ico'
foreach ($file in @($wxs, $icon)) {
    if (-not (Test-Path -LiteralPath $file -PathType Leaf)) {
        throw "the installer needs $file, which is missing"
    }
}
if (-not (Get-Command dotnet -ErrorAction SilentlyContinue)) {
    throw 'dotnet is not on PATH; the installer is built with WiX, which runs on .NET (docs/RELEASING.md)'
}

# What the installer may hold, which the read-back below checks; see
# there. The tables are the ones WiX writes for packaging\windows\
# codepp.wxs as it stands: files, a shortcut, registry values, the
# upgrade rule, and the tables of its UI. A table that is not among
# them is how an installer does something else: installs a service,
# registers a COM class, sets an environment variable, changes
# permissions, deletes a file. The registry keys are the ones the .wxs
# writes, as patterns, each below a root that follows the kind of
# installation.
$customActionsAllowed = @('WixUIPrintEula_X64')
$tablesAllowed = @(
    '_Validation', 'AdminExecuteSequence', 'AdminUISequence', 'AdvtExecuteSequence', 'Binary',
    'CheckBox', 'Component', 'Control', 'ControlCondition', 'ControlEvent', 'CustomAction',
    'Dialog', 'Directory', 'EventMapping', 'Feature', 'FeatureComponents', 'File', 'Icon',
    'InstallExecuteSequence', 'InstallUISequence', 'LaunchCondition', 'ListBox', 'Media',
    'MsiFileHash', 'Property', 'RadioButton', 'Registry', 'Shortcut', 'TextStyle', 'UIText',
    'Upgrade')
$registryKeysAllowed = @(
    '\ASoftware\\Microsoft\\Windows\\CurrentVersion\\App Paths\\codepp\.exe\z',
    '\ASoftware\\Classes\\Applications\\codepp\.exe(\\shell\\open\\command)?\z',
    '\ASoftware\\Classes\\Codepp\.Document\\shell\\open\\command\z',
    '\ASoftware\\Classes\\\*\\shell\\EditWithCodepp(\\command)?\z',
    '\ASoftware\\Classes\\\.[a-z0-9]+\\OpenWithProgids\z',
    '\ASoftware\\Code\+\+\\Installer\z')

# Windows Installer takes a version of numbers only, so the installer
# gets the version without the part a manual run's build has after
# x.y.z, and says the whole of it in its name instead. Each of the three numbers has an upper bound.
$productVersion = ($version -split '-', 2)[0]
if ($productVersion -notmatch '\A([0-9]{1,5})\.([0-9]{1,5})\.([0-9]{1,5})\z' -or
    [int]$Matches[1] -gt 255 -or [int]$Matches[2] -gt 255 -or [int]$Matches[3] -gt 65535) {
    throw "the version '$version' does not begin with an x.y.z Windows Installer takes (at most 255.255.65535)"
}
if ($version -eq $productVersion) {
    $productName = 'Code++'
}
else {
    $productName = "Code++ $version"
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
Add-Type -AssemblyName System.IO.Compression.FileSystem
try {
    # WiX first, for the same reason as the checks above: a package
    # that does not arrive, or arrives changed, stops the script before
    # it has written anything.
    $tools = Join-Path $work 'tools'
    New-Item -ItemType Directory -Force -Path $tools | Out-Null
    $wixDll = Get-OnlyFile (Get-VerifiedPackage 'wix' $wixVersion $wixPackages['wix'] $tools) 'wix.dll'
    $wixUi = Get-OnlyFile (
        Get-VerifiedPackage 'wixtoolset.ui.wixext' $wixVersion $wixPackages['wixtoolset.ui.wixext'] $tools
    ) 'WixToolset.UI.wixext.dll'
    # And the updater's library, into a folder of its own: it goes into
    # the installer and not into the zip.
    $updater = Join-Path $work 'updater'
    $updaterDll = Get-WinSparkleDll $winSparkleVersion $winSparkleZipSha256 $winSparkleDllSha256 $tools $updater

    foreach ($binary in $binaries) {
        Copy-Item -LiteralPath $binary -Destination $stage
    }
    # `.txt`, so that a double click opens the licence in Notepad.
    Copy-Item -LiteralPath 'LICENSE' -Destination (Join-Path $stage 'LICENSE.txt')
    Copy-Item -LiteralPath 'THIRD_PARTY_LICENSES.md' -Destination $stage
    Copy-Item -LiteralPath $Notices -Destination (Join-Path $stage 'THIRD_PARTY_CRATES.md')

    # Both files are made here, in the temporary folder, and moved to
    # <OutDir> together at the end: an installer that fails to build,
    # or fails the read-back below, leaves nothing there, the zip
    # included.
    $built = Join-Path $work 'out'
    New-Item -ItemType Directory -Force -Path $built | Out-Null
    $archive = Join-Path $built "$name.zip"
    # .NET's own zip writer rather than Compress-Archive, which in
    # PowerShell 5.1 reads every file into memory and has written
    # backslashes into entry names.
    [System.IO.Compression.ZipFile]::CreateFromDirectory(
        $stage, $archive, [System.IO.Compression.CompressionLevel]::Optimal, $false)

    # The installer, from the same folder of files and the updater's.
    $aux = Join-Path $work 'installer'
    New-Item -ItemType Directory -Force -Path $aux | Out-Null
    $licence = ConvertTo-LicenceRtf -Text (Get-Content -LiteralPath 'LICENSE' -Raw -Encoding UTF8)
    [System.IO.File]::WriteAllText((Join-Path $aux 'License.rtf'), $licence, [System.Text.Encoding]::ASCII)

    $installer = Join-Path $built "$name.msi"
    $env:DOTNET_CLI_TELEMETRY_OPTOUT = '1'
    $env:DOTNET_NOLOGO = '1'
    # The extension by its file, so that WiX does not look one up in
    # the account's extension cache.
    Invoke-Tool 'wix build' 'dotnet' @(
        $wixDll, 'build', $wxs,
        '-arch', 'x64',
        '-ext', $wixUi,
        '-culture', 'en-US',
        '-d', "ProductName=$productName",
        '-d', "ProductVersion=$productVersion",
        '-d', "FullVersion=$version",
        '-d', "Icon=$((Resolve-Path -LiteralPath $icon).Path)",
        '-bindpath', $stage,
        '-bindpath', $updater,
        '-bindpath', $aux,
        '-intermediatefolder', (Join-Path $work 'wix'),
        '-pdbtype', 'none',
        '-out', $installer)

    # Read the installer back: that it carries the version it is named
    # for, installs for the whole machine, holds exactly the files the
    # zip holds and the updater's library, each at its size and with
    # its bytes, runs no code of its own while installing, and holds no
    # table and writes no registry key but the expected ones. Windows
    # Installer stores a file's name as `short|long` when the name does
    # not fit 8.3.
    $found = @(Get-MsiRows $installer "SELECT ``Value`` FROM ``Property`` WHERE ``Property`` = 'ProductVersion'")
    if ($found.Count -ne 1 -or $found[0] -ne $productVersion) {
        throw "the installer says its version is '$($found -join ', ')', expected '$productVersion'"
    }
    $found = @(Get-MsiRows $installer "SELECT ``Value`` FROM ``Property`` WHERE ``Property`` = 'ALLUSERS'")
    if ($found.Count -ne 1 -or $found[0] -ne '1') {
        throw "the installer does not install for the whole machine (ALLUSERS is '$($found -join ', ')')"
    }
    $found = @(Get-MsiRows $installer 'SELECT `FileName`, `FileSize` FROM `File`' -Columns 2)
    $packed = @($found | ForEach-Object {
            $cells = $_ -split "`t"
            "$(($cells[0] -split '\|')[-1]) ($($cells[1]) bytes)"
        } | Sort-Object)
    $staged = @(@(Get-ChildItem -LiteralPath $stage -File) + @(Get-Item -LiteralPath $updaterDll) |
            ForEach-Object { "$($_.Name) ($($_.Length) bytes)" } | Sort-Object)
    if (Compare-Object $staged $packed -CaseSensitive) {
        throw "the installer holds '$($packed -join ', ')', but the zip's files and the updater's library are '$($staged -join ', ')'; packaging\windows\codepp.wxs lists the files to install"
    }

    # And byte for byte, which names and sizes do not show: WiX takes
    # the installer's files out again, each under its key in the File
    # table, and each must have the SHA-256 of the file it was made
    # from. For the updater's library that is the hash written above,
    # so the installer holds the pinned bytes whatever happened to the
    # downloaded file between its check and the build.
    $unpacked = Join-Path $work 'readback'
    Invoke-Tool 'wix msi decompile' 'dotnet' @(
        $wixDll, 'msi', 'decompile', $installer,
        '-x', $unpacked,
        '-intermediatefolder', (Join-Path $work 'wix-readback'),
        '-out', (Join-Path $work 'readback.wxs'))
    $found = @(Get-MsiRows $installer 'SELECT `File`, `FileName` FROM `File`' -Columns 2)
    foreach ($row in $found) {
        $cells = $row -split "`t"
        $fileName = ($cells[1] -split '\|')[-1]
        if ($fileName -ceq (Split-Path -Leaf $updaterDll)) {
            $expected = $winSparkleDllSha256
        }
        else {
            # The name is one of the stage folder's: the comparison
            # above has just required that.
            $expected = (Get-FileHash -LiteralPath (Join-Path $stage $fileName) -Algorithm SHA256).Hash
        }
        $inside = Join-Path (Join-Path $unpacked 'File') $cells[0]
        if (-not (Test-Path -LiteralPath $inside -PathType Leaf)) {
            throw "WiX did not hand back the installer's $fileName to compare"
        }
        $actual = (Get-FileHash -LiteralPath $inside -Algorithm SHA256).Hash
        if ($actual -ne $expected) {
            throw "the installer's $fileName has SHA-256 $actual, not the $expected of the file it was made from"
        }
    }

    # The one custom action is the UI extension's, behind the
    # licence page's Print button. It runs in the installer's own
    # window, as whoever started it. Nothing but Windows Installer's
    # standard actions may be in a sequence, the elevated part of
    # the installation included: an action added there, by a change
    # to the .wxs or by anything else, stops the release here.
    $custom = @()
    if (@(Get-MsiRows $installer "SELECT ``Name`` FROM ``_Tables`` WHERE ``Name`` = 'CustomAction'").Count -eq 1) {
        $custom = @(Get-MsiRows $installer 'SELECT `Action` FROM `CustomAction`')
    }
    if ((@($custom | Sort-Object) -join ', ') -ne ($customActionsAllowed -join ', ')) {
        throw "the installer's custom actions are '$($custom -join ', ')', expected '$($customActionsAllowed -join ', ')'"
    }
    foreach ($sequence in 'InstallExecuteSequence', 'InstallUISequence', 'AdminExecuteSequence', 'AdminUISequence', 'AdvtExecuteSequence') {
        if (@(Get-MsiRows $installer "SELECT ``Name`` FROM ``_Tables`` WHERE ``Name`` = '$sequence'").Count -ne 1) {
            continue
        }
        $found = @(Get-MsiRows $installer "SELECT ``Action`` FROM ``$sequence``")
        $sequenced = @($found | Where-Object { $custom -contains $_ })
        if ($sequenced.Count -ne 0) {
            throw "the installer runs the custom action '$($sequenced -join ', ')' in its $sequence"
        }
    }

    $found = @(Get-MsiRows $installer 'SELECT `Name` FROM `_Tables`')
    $extra = @($found | Where-Object { $tablesAllowed -notcontains $_ } | Sort-Object)
    if ($extra.Count -ne 0) {
        throw "the installer holds the table '$($extra -join ', ')', which this script does not expect of packaging\windows\codepp.wxs; if the change is meant, add it to the list here"
    }
    $found = @(Get-MsiRows $installer 'SELECT `Key` FROM `Registry`')
    $extra = @($found | Where-Object {
            $key = $_
            @($registryKeysAllowed | Where-Object { $key -cmatch $_ }).Count -eq 0
        } | Sort-Object -Unique)
    if ($extra.Count -ne 0) {
        throw "the installer writes the registry key '$($extra -join ', ')', which this script does not expect of packaging\windows\codepp.wxs; if the change is meant, add it to the list here"
    }

    New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
    $OutDir = (Resolve-Path -LiteralPath $OutDir).Path
    foreach ($file in @($archive, $installer)) {
        Move-Item -LiteralPath $file -Destination $OutDir -Force
    }
    $archive = Join-Path $OutDir "$name.zip"
    $installer = Join-Path $OutDir "$name.msi"
}
finally {
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host "packaged $archive and $installer"
Get-Item -LiteralPath $archive, $installer | Format-List Name, Length
