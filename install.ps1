# Install Graphicle's two programs, graphicle.exe and graphicle-graph.exe, on
# Windows (x86-64, preview) from a GitHub release of graphicle-ai/graphicle.
# In PowerShell (Windows PowerShell 5.1 or PowerShell 7):
#
#   irm https://github.com/graphicle-ai/graphicle/releases/latest/download/install.ps1 | iex
#
# It downloads graphicle-<version>-x86_64-pc-windows-msvc.zip and its .sha256,
# checks the SHA-256 sum before installing anything, copies both programs into
# one folder and adds that folder to your user PATH. It never needs
# administrator rights and changes nothing else.
#
# Options, as parameters of a saved copy (powershell -ExecutionPolicy Bypass
# -File .\install.ps1 -Version 0.1.1) or as the environment variable beside
# each, set before the line above:
#   -Version VERSION   GRAPHICLE_VERSION      the release to install (default: the latest)
#   -InstallDir DIR    GRAPHICLE_INSTALL_DIR  install into DIR
#                                             (default: %LOCALAPPDATA%\Programs\Graphicle\bin)
#   -BaseUrl URL       GRAPHICLE_BASE_URL     folder holding the release files, a URL or
#                                             a local folder (default: the GitHub release
#                                             of VERSION)
#   -NoModifyPath                             leave the user PATH alone
#
# To uninstall, run `graphicle uninstall`, which removes what Graphicle added
# to your agents and repositories, then delete the folder and remove it from
# your user PATH.
[CmdletBinding()]
param(
    [string]$Version = $env:GRAPHICLE_VERSION,
    [string]$InstallDir = $env:GRAPHICLE_INSTALL_DIR,
    [string]$BaseUrl = $env:GRAPHICLE_BASE_URL,
    [switch]$NoModifyPath,
    [switch]$Help
)

# A child scope, so that run with `iex` the preferences below stay out of the
# caller's session. Errors are thrown, never `exit`, which would close it.
& {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'
    $Repository = 'graphicle-ai/graphicle'
    $Target = 'x86_64-pc-windows-msvc'
    $Programs = @('graphicle.exe', 'graphicle-graph.exe')

    function Say([string]$Message) {
        Write-Host "graphicle-install: $Message"
    }

    function Fail([string]$Message) {
        throw "graphicle-install: error: $Message"
    }

    if ($Help) {
        Write-Host @"
Install graphicle and graphicle-graph from a release of $Repository.

Usage: install.ps1 [-Version VERSION] [-InstallDir DIR] [-BaseUrl URL] [-NoModifyPath]

  -Version VERSION  the release to install (default: the latest)        GRAPHICLE_VERSION
  -InstallDir DIR   install into DIR                                    GRAPHICLE_INSTALL_DIR
                    (default: %LOCALAPPDATA%\Programs\Graphicle\bin)
  -BaseUrl URL      folder holding the release files, a URL or a        GRAPHICLE_BASE_URL
                    local folder (default: the GitHub release of VERSION)
  -NoModifyPath     leave the user PATH alone

The archive's SHA-256 sum is checked before anything is installed.
Administrator rights are never needed. To uninstall, run 'graphicle uninstall',
then delete the folder and remove it from your user PATH.

After installing: cd your-repo; graphicle setup
"@
        return
    }

    if ($PSVersionTable.PSVersion.Major -lt 5) {
        Fail "this installer needs Windows PowerShell 5.1 or PowerShell 7; this is $($PSVersionTable.PSVersion)"
    }
    # Windows PowerShell on an older .NET may not offer TLS 1.2, which GitHub
    # needs; the system default (0) already does.
    $protocols = [Net.ServicePointManager]::SecurityProtocol
    if ($protocols -ne 0 -and -not ($protocols -band [Net.SecurityProtocolType]::Tls12)) {
        [Net.ServicePointManager]::SecurityProtocol = $protocols -bor [Net.SecurityProtocolType]::Tls12
    }

    # Copies NAME from the release folder, a URL or a local folder, to DESTINATION.
    function Fetch([string]$Name, [string]$Destination) {
        if ($BaseUrl -match '^https?://') {
            Invoke-WebRequest -Uri "$BaseUrl/$Name" -OutFile $Destination -UseBasicParsing
        } else {
            $source = if ($BaseUrl -match '^file:') { ([Uri]$BaseUrl).LocalPath } else { $BaseUrl }
            Copy-Item -LiteralPath (Join-Path $source $Name) -Destination $Destination
        }
    }

    $architecture = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
    if ($architecture -ne 'AMD64') {
        Fail "no prebuilt programs for $architecture Windows; build from source: https://github.com/$Repository"
    }

    $Version = "$Version".Trim()
    if ($Version.StartsWith('v')) { $Version = $Version.Substring(1) }
    if (-not $Version) {
        # The version of the latest release, from the address /releases/latest
        # redirects to (.../releases/tag/v<version>); no API call, so no rate limit.
        $latest = "https://github.com/$Repository/releases/latest"
        try {
            $response = Invoke-WebRequest -Uri $latest -Method Head -UseBasicParsing
        } catch {
            Fail "found no published release at $latest (or could not reach it); pass -Version"
        }
        # Windows PowerShell 5.1 gives an HttpWebResponse, PowerShell 7 an HttpResponseMessage.
        $final = $null
        if ($response.BaseResponse.PSObject.Properties['ResponseUri']) {
            $final = [string]$response.BaseResponse.ResponseUri
        } elseif ($response.BaseResponse.PSObject.Properties['RequestMessage']) {
            $final = [string]$response.BaseResponse.RequestMessage.RequestUri
        }
        $tag = ("$final".TrimEnd('/') -split '/')[-1]
        if ($tag -notmatch '^v[0-9]') {
            Fail "found no published release at $latest (it led to '$final'); pass -Version"
        }
        $Version = $tag.Substring(1)
    }
    if ($Version -notmatch '^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$') {
        Fail "'$Version' is not a release version such as 0.1.0"
    }

    if (-not $InstallDir) {
        if (-not $env:LOCALAPPDATA) { Fail 'LOCALAPPDATA is not set; pass -InstallDir' }
        $InstallDir = Join-Path $env:LOCALAPPDATA 'Programs\Graphicle\bin'
    }
    $InstallDir = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($InstallDir)
    if (-not $BaseUrl) { $BaseUrl = "https://github.com/$Repository/releases/download/v$Version" }
    $BaseUrl = $BaseUrl.TrimEnd('/')
    $archive = "graphicle-$Version-$Target.zip"

    $temporary = Join-Path ([IO.Path]::GetTempPath()) ('graphicle-install-' + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $temporary | Out-Null
    try {
        Say "downloading Graphicle $Version for $Target"
        foreach ($file in @($archive, "$archive.sha256")) {
            try {
                Fetch $file (Join-Path $temporary $file)
            } catch {
                Fail "could not download $BaseUrl/$file ($($_.Exception.Message))"
            }
        }

        $line = (Get-Content -LiteralPath (Join-Path $temporary "$archive.sha256") -TotalCount 1)
        if (-not ("$line" -match '^([0-9A-Fa-f]{64})[ *]+(\S+)\s*$')) { Fail "$archive.sha256 holds no SHA-256 sum" }
        $expected = $Matches[1].ToLowerInvariant()
        $listed = $Matches[2].TrimStart('*')
        if ($listed -ne $archive) { Fail "$archive.sha256 names '$listed', not $archive" }
        $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $temporary $archive)).Hash.ToLowerInvariant()
        if ($actual -ne $expected) {
            Fail "checksum mismatch for ${archive}: expected $expected, got $actual; nothing was installed"
        }
        Say "checksum verified ($expected)"

        $unpacked = Join-Path $temporary 'unpacked'
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        try {
            [IO.Compression.ZipFile]::ExtractToDirectory((Join-Path $temporary $archive), $unpacked)
        } catch {
            Fail "could not unpack ${archive}: $($_.Exception.Message)"
        }
        $folder = Join-Path $unpacked "graphicle-$Version-$Target"
        foreach ($program in $Programs) {
            if (-not (Test-Path -LiteralPath (Join-Path $folder $program) -PathType Leaf)) {
                Fail "$archive holds no $program"
            }
        }

        try {
            New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
        } catch {
            Fail "could not create $InstallDir; choose another folder with -InstallDir"
        }
        foreach ($program in $Programs) {
            $destination = Join-Path $InstallDir $program
            # Leftovers of earlier installs whose program was running then.
            Get-ChildItem -LiteralPath $InstallDir -Filter "$program.old-*" -ErrorAction SilentlyContinue |
                Remove-Item -Force -ErrorAction SilentlyContinue
            # Copy beside the destination first, so an interrupted copy leaves
            # no broken program. Windows will not overwrite a running program
            # but lets it be renamed, so the old one moves aside, and is deleted
            # if it is not running.
            $staged = Join-Path $InstallDir ".$program.new"
            try {
                Copy-Item -LiteralPath (Join-Path $folder $program) -Destination $staged -Force
            } catch {
                Fail "could not write to $InstallDir; choose another folder with -InstallDir ($($_.Exception.Message))"
            }
            $aside = $null
            if (Test-Path -LiteralPath $destination) {
                $aside = Join-Path $InstallDir ("$program.old-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
                try {
                    Move-Item -LiteralPath $destination -Destination $aside
                } catch {
                    Remove-Item -LiteralPath $staged -Force -ErrorAction SilentlyContinue
                    Fail "could not replace ${destination}: $($_.Exception.Message)"
                }
            }
            try {
                Move-Item -LiteralPath $staged -Destination $destination
            } catch {
                if ($aside) { Move-Item -LiteralPath $aside -Destination $destination -ErrorAction SilentlyContinue }
                Fail "could not install ${destination}: $($_.Exception.Message)"
            }
            if ($aside) { Remove-Item -LiteralPath $aside -Force -ErrorAction SilentlyContinue }
        }
        Say "installed graphicle.exe and graphicle-graph.exe in $InstallDir"
    } finally {
        Remove-Item -LiteralPath $temporary -Recurse -Force -ErrorAction SilentlyContinue
    }

    $graphicle = Join-Path $InstallDir 'graphicle.exe'
    try {
        $reported = & $graphicle --version 2>&1 | Out-String
        $code = $LASTEXITCODE
    } catch {
        Fail "$graphicle does not run here: $($_.Exception.Message)"
    }
    if ($code -ne 0) {
        Fail "$graphicle does not run here (exit code $code): $("$reported".Trim())"
    }
    Say ("$reported".Trim())

    # The user PATH, read and written raw so that entries such as
    # %USERPROFILE%\bin keep their variables and the value keeps its type.
    $normalized = $InstallDir.TrimEnd('\')
    $onPath = {
        param([string]$List)
        foreach ($entry in ($List -split ';')) {
            $entry = [Environment]::ExpandEnvironmentVariables($entry.Trim()).TrimEnd('\')
            if ($entry -and $entry -ieq $normalized) { return $true }
        }
        return $false
    }
    if ($NoModifyPath) {
        if (-not (& $onPath $env:Path)) { Say "add $InstallDir to your PATH" }
    } else {
        # Opens HKCU\Environment for writing (and creates it if a profile lacks it).
        $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment')
        try {
            $raw = $key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
            if (-not (& $onPath $raw)) {
                $kind = if ($raw) { $key.GetValueKind('Path') } else { [Microsoft.Win32.RegistryValueKind]::ExpandString }
                $value = if ($raw) { "$($raw.TrimEnd(';'));$normalized" } else { $normalized }
                $key.SetValue('Path', $value, $kind)
                # Tell running programs (Explorer, new terminals) that the
                # environment changed: setting a user variable through .NET
                # broadcasts WM_SETTINGCHANGE.
                [Environment]::SetEnvironmentVariable('GRAPHICLE_INSTALL_PATH_CHANGED', '1', 'User')
                [Environment]::SetEnvironmentVariable('GRAPHICLE_INSTALL_PATH_CHANGED', [NullString]::Value, 'User')
                Say "added $InstallDir to your user PATH; new terminals will find graphicle"
            }
        } finally {
            $key.Close()
        }
        if (-not (& $onPath $env:Path)) { $env:Path = "$($env:Path.TrimEnd(';'));$normalized" }
    }
    Say 'By default, graphicle checks GitHub for a newer release at most once a day; to turn this off, run: graphicle config set update_notifier false'
    Say 'Next: cd your-repo; graphicle setup'
}
