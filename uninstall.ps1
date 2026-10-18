# Removes user-memory from this Windows machine: what setup installed for
# the clients (through the installed binary's `uninstall`), the Docker
# server the installer set up (through its `server remove`), then the
# binary. When the installed binary is missing or does not run, the latest
# release's, checked against its SHA-256, stands in for it. Memories stay
# unless the user confirms deleting them. A shared server elsewhere is never
# touched. The Windows twin of uninstall.sh.
#
#   irm https://github.com/l7aromeo/user-memory/releases/latest/download/uninstall.ps1 | iex
#   & ([scriptblock]::Create((irm https://github.com/l7aromeo/user-memory/releases/latest/download/uninstall.ps1))) -DryRun
#
# It runs on Windows PowerShell 5.1 and PowerShell 7.

# PSScriptAnalyzer: the parameters are read inside the child scope below,
# which the analyzer does not follow; and the script talks to the person at
# the console, so its messages go to the host (the information stream since
# PowerShell 5), never into the output that functions return.
[Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSReviewUnusedParameter', '',
    Justification = 'Read inside the child scope, which the analyzer does not follow.')]
[Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSAvoidUsingWriteHost', '',
    Justification = 'Console messages for the person running the uninstaller; function output carries return values only.')]
param(
    [switch]$DryRun,
    [switch]$Purge,
    [switch]$Yes,
    [switch]$NoExport,
    [string]$Client,
    [string]$InstallDir,
    [switch]$Help
)

# Everything runs in a child scope, so a paste into a console (`irm | iex`)
# leaves no functions, preferences or strict mode behind in it.
& {
    $ErrorActionPreference = 'Stop'
    # Invoke-WebRequest's progress bar slows Windows PowerShell's downloads
    # many times over.
    $ProgressPreference = 'SilentlyContinue'
    Set-StrictMode -Version 2.0

    # Exit codes, as uninstall.sh and `user-memory uninstall` use them:
    # items were left and named, or the run was refused before any change.
    $ExitLeft = 1
    $ExitRefused = 2
    # A release download that keeps failing still fails in seconds; a single
    # reset transfer (as GitHub's CDN sometimes does) is retried.
    $FetchRetries = 3
    $FetchRetryDelaySeconds = 1
    # Every failure this script reports starts with this.
    $Prefix = 'user-memory uninstall: '
    # What the run has found so far, shared by the functions below.
    $state = @{ Status = 0 }

    function Fail([string]$Message) {
        throw "$Prefix$Message"
    }

    function Show-Usage {
        @'
Remove user-memory from this machine.

Usage: & ([scriptblock]::Create((irm https://github.com/l7aromeo/user-memory/releases/latest/download/uninstall.ps1))) [options]

  -DryRun               list what would be removed or kept; change nothing
  -Purge                delete the memories too (the local store and its
                        backups, or the Docker server's volume); in a
                        console it asks anyway
  -Yes                  with -Purge: delete without asking
  -NoExport             with -Purge: skip the export taken first
  -Client NAME          auto (default), codex, claude or both
  -InstallDir DIR       where the binary is
                        (default %LOCALAPPDATA%\Programs\user-memory\bin)

Memories are kept unless you confirm: in a console you are asked (the
default keeps them, and deleting offers an export first and asks you to
type delete); without one, -Purge -Yes deletes them. When the installed
user-memory is missing or does not run, the latest release's stands in
for it, downloaded and checked against its SHA-256.
'@
    }

    # The exit code a native call reads as when no program ran: one that
    # could not start, or a file PowerShell handed to the system to open
    # instead of running it. Any code but 0 is a failure.
    $NotStarted = -1

    # Runs COMMAND, a native program call. Windows PowerShell can turn a
    # native program's error output into errors, which must not stop the
    # run: only its exit code counts. That code is preset, so a call that
    # runs no program reads as failed, never as unset; a program that
    # cannot start says why and leaves it so.
    function Invoke-Native([scriptblock]$Command) {
        $ErrorActionPreference = 'Continue'
        $global:LASTEXITCODE = $NotStarted
        try {
            & $Command
        } catch {
            [Console]::Error.WriteLine($_.Exception.Message)
        }
    }

    # Whether Windows starts BINARY and it answers --version. A program
    # that Smart App Control or an antivirus blocks never starts. The
    # process is started directly, never through the shell, so a file that
    # is no program fails here on every system instead of being opened.
    function Test-Runnable([string]$Binary) {
        $start = New-Object System.Diagnostics.ProcessStartInfo
        $start.FileName = $Binary
        $start.Arguments = '--version'
        $start.UseShellExecute = $false
        $start.RedirectStandardOutput = $true
        $start.RedirectStandardError = $true
        try {
            $process = [System.Diagnostics.Process]::Start($start)
        } catch {
            return $false
        }
        # The version is one short line, so reading one stream after the
        # other cannot fill the other's pipe.
        $null = $process.StandardOutput.ReadToEnd()
        $null = $process.StandardError.ReadToEnd()
        $process.WaitForExit()
        return ($process.ExitCode -eq 0)
    }

    # The switches this PowerShell was started with, lowercased and without
    # their leading - or /.
    function Get-HostSwitch {
        $names = @()
        foreach ($argument in @([Environment]::GetCommandLineArgs() | Select-Object -Skip 1)) {
            if ($argument -match '^[-/]([A-Za-z]+)$') { $names += $Matches[1].ToLowerInvariant() }
        }
        return $names
    }

    # Whether NAME abbreviates the PowerShell switch FULL as the host
    # accepts it: at least MINIMUM letters long.
    function Test-Switch([string]$Name, [string]$Full, [int]$Minimum) {
        return ($Name.Length -ge $Minimum -and $Full.StartsWith($Name))
    }

    # Whether this run can ask questions: a console a person types into, not
    # a -NonInteractive host or redirected input.
    function Test-Interactive {
        if (-not [Environment]::UserInteractive) { return $false }
        if ([Console]::IsInputRedirected) { return $false }
        foreach ($name in Get-HostSwitch) {
            if (Test-Switch -Name $name -Full 'noninteractive' -Minimum 4) { return $false }
        }
        return $true
    }

    # Whether `exit` may end this run: run as a file, or by a host that ends
    # once its command is done (-Command, -File, -EncodedCommand or
    # -NonInteractive, without -NoExit). In a console the user pasted into,
    # `exit` would close the console.
    function Test-MayExit {
        if (Get-Variable -Name PSCommandPath -ValueOnly -ErrorAction SilentlyContinue) { return $true }
        $ends = $false
        foreach ($name in Get-HostSwitch) {
            if (Test-Switch -Name $name -Full 'noexit' -Minimum 3) { return $false }
            if ((Test-Switch -Name $name -Full 'command' -Minimum 1) -or (Test-Switch -Name $name -Full 'file' -Minimum 1) -or
                (Test-Switch -Name $name -Full 'encodedcommand' -Minimum 1) -or $name -eq 'ec' -or
                (Test-Switch -Name $name -Full 'noninteractive' -Minimum 4)) {
                $ends = $true
            }
        }
        return $ends
    }

    # Ends the run with STATUS, leaving a pasted-into console open.
    function Exit-Run([int]$Status) {
        if ($Status -eq 0) { return }
        if (-not (Test-MayExit)) {
            $global:LASTEXITCODE = $Status
            return
        }
        exit $Status
    }

    # The client build for this machine. An x64 PowerShell on ARM64 Windows
    # reports AMD64 and names the real processor in PROCESSOR_ARCHITEW6432.
    function Get-Target {
        $machine = $env:PROCESSOR_ARCHITEW6432
        if (-not $machine) { $machine = $env:PROCESSOR_ARCHITECTURE }
        switch ($machine) {
            'ARM64' { return 'aarch64-pc-windows-msvc' }
            'AMD64' { return 'x86_64-pc-windows-msvc' }
        }
        Fail "there is no client build for Windows on $machine"
    }

    # Downloads URI to FILE, retrying any failure; returns whether it worked.
    function Get-Download([string]$Uri, [string]$File) {
        $attempt = 0
        while ($true) {
            try {
                Invoke-WebRequest -Uri $Uri -OutFile $File -UseBasicParsing -ErrorAction Stop
                return $true
            } catch {
                if ($attempt -ge $FetchRetries) { return $false }
                $attempt++
                Start-Sleep -Seconds $FetchRetryDelaySeconds
            }
        }
    }

    # The SHA-256 of FILE in lowercase hex, with .NET directly: Get-FileHash
    # can fail to load in PowerShell 7 started from Windows PowerShell.
    function Get-Sha256([string]$File) {
        $sha = [Security.Cryptography.SHA256]::Create()
        $stream = [IO.File]::OpenRead($File)
        try {
            return ([BitConverter]::ToString($sha.ComputeHash($stream)) -replace '-', '').ToLowerInvariant()
        } finally {
            $stream.Dispose()
            $sha.Dispose()
        }
    }

    # Downloads the latest release's client into FOLDER, checked against its
    # SHA-256 as install.ps1 checks it, and returns its path once Windows
    # has started it.
    function Get-ReleaseBinary([string]$Folder) {
        $base = $env:USER_MEMORY_RELEASES
        if (-not $base) { $base = 'https://github.com/l7aromeo/user-memory/releases' }
        $base = $base.TrimEnd('/')
        if ($base -notmatch '^https://' -and $base -notmatch '^http://(127\.0\.0\.1|localhost|\[::1\])(:[0-9]+)?(/|$)') {
            Fail "the releases address $base uses plain HTTP; use https:// (plain http is allowed only for this machine)"
        }
        $name = "user-memory-$(Get-Target).exe"
        $assets = "$base/latest/download"
        New-Item -ItemType Directory -Path $Folder -ErrorAction Stop | Out-Null
        $file = Join-Path $Folder 'user-memory.exe'
        if (-not (Get-Download "$assets/$name" $file)) { Fail "cannot download $assets/$name" }
        if (-not (Get-Download "$assets/$name.sha256" "$file.sha256")) { Fail "cannot download $assets/$name.sha256" }
        $expected = ([string](Get-Content -LiteralPath "$file.sha256" -TotalCount 1 -ErrorAction Stop) -split '\s+')[0]
        if ($expected -notmatch '^[0-9a-f]{64}$') { Fail "the release's checksum file is malformed" }
        if ((Get-Sha256 $file) -ne $expected) { Fail 'the download does not match its checksum' }
        # The binary is not code-signed yet, so Windows may refuse to start it.
        if (-not (Test-Runnable $file)) {
            Fail 'Windows did not run the downloaded user-memory.exe; Smart App Control or an antivirus may block it, as it is not code-signed yet'
        }
        return $file
    }

    function Get-AbsolutePath([string]$Path) {
        return $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Path)
    }

    # Prints a removed item, or the item a dry run would remove.
    function Write-Removed([string]$Item) {
        if ($DryRun) { Write-Host "  Would remove: $Item" } else { Write-Host "  Removed: $Item" }
    }

    # Reports an item of ours that could not be removed, with what to do;
    # the run then exits with ExitLeft after everything else.
    function Write-Left([string]$Item) {
        Write-Host "  Left: $Item"
        $state.Status = $ExitLeft
    }

    # Removes FOLDER when nothing is left in it.
    function Remove-EmptyFolder {
        [Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSUseShouldProcessForStateChangingFunctions', '',
            Justification = 'An internal step of this script, whose -DryRun is its what-if.')]
        param([string]$Folder)
        if ((Test-Path -LiteralPath $Folder -PathType Container) -and
            -not (Get-ChildItem -LiteralPath $Folder -Force -ErrorAction SilentlyContinue)) {
            Remove-Item -LiteralPath $Folder -Force -ErrorAction SilentlyContinue
        }
    }

    # Deletes the binary FILE. A running agent session holds it open, and
    # Windows deletes no running .exe, so then it is left with what to do.
    function Remove-Binary {
        [Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSUseShouldProcessForStateChangingFunctions', '',
            Justification = 'An internal step of this script, whose -DryRun is its what-if.')]
        param([string]$File)
        if ($DryRun) {
            Write-Removed "the user-memory binary $File"
            return
        }
        try {
            Remove-Item -LiteralPath $File -Force -ErrorAction Stop
            Write-Removed "the user-memory binary $File"
        } catch {
            Write-Left "$File, in use by a running agent session; close Claude Code and Codex, then delete it"
        }
    }

    function Invoke-Uninstall {
        if ($Help) {
            Show-Usage
            return
        }
        $state.Status = 0
        $clientName = $Client
        if (-not $clientName) { $clientName = 'auto' }
        if ($clientName -notin @('auto', 'codex', 'claude', 'both')) { Fail '-Client takes auto, codex, claude or both' }
        if ($Yes -and -not $Purge) { Fail '-Yes goes with -Purge' }
        if ($NoExport -and -not $Purge) { Fail '-NoExport goes with -Purge' }
        if ($Purge -and -not $Yes -and -not $DryRun -and -not (Test-Interactive)) {
            Fail '-Purge deletes your memories; without a console to confirm, it also needs -Yes. Nothing was changed.'
        }
        $defaultDirectory = Join-Path $env:LOCALAPPDATA 'Programs\user-memory\bin'
        $installDirectory = $defaultDirectory
        if ($InstallDir) { $installDirectory = Get-AbsolutePath $InstallDir }
        $binary = Join-Path $installDirectory 'user-memory.exe'
        $dataDir = Join-Path $env:LOCALAPPDATA 'user-memory'
        $dockerName = $env:USER_MEMORY_DOCKER_NAME
        if (-not $dockerName) { $dockerName = 'user-memory' }
        if ($dockerName -notmatch '^[a-z0-9][a-z0-9_-]*$') { Fail 'USER_MEMORY_DOCKER_NAME must be a lowercase Docker project name' }
        $dockerFolder = 'docker'
        if ($dockerName -ne 'user-memory') { $dockerFolder = "docker-$dockerName" }
        $dockerDir = Join-Path $dataDir $dockerFolder

        # The memory choices, passed on to both of the binary's removals.
        $choices = @()
        if ($DryRun) { $choices += '--dry-run' }
        if ($Purge) { $choices += '--purge' }
        if ($Yes) { $choices += '--yes' }
        if ($NoExport) { $choices += '--no-export' }

        $compose = Join-Path $dockerDir 'compose.yaml'
        $runs = $false
        if (Test-Path -LiteralPath $binary -PathType Leaf) {
            $runs = Test-Runnable $binary
        }
        # The binary that removes the rest: the installed one, or the latest
        # release's when that is missing or broken. It holds every path and
        # format setup wrote, so this script repeats none of them.
        $tool = $binary
        if (-not $runs) {
            $traces = @($binary, (Join-Path $installDirectory '.user-memory.json'), $compose, $dataDir)
            if (-not @($traces | Where-Object { Test-Path -LiteralPath $_ })) {
                Write-Host 'Uninstalling user memory...'
                Write-Host "  Nothing to remove: no user-memory in $installDirectory (pass -InstallDir for another folder)."
                return
            }
            Write-Host 'The installed user-memory is missing or does not run; using the latest release to remove what is left.'
            $state.Temporary = Join-Path ([IO.Path]::GetTempPath()) ('user-memory-uninstall-' + [Guid]::NewGuid().ToString('N'))
            try {
                $tool = Get-ReleaseBinary $state.Temporary
            } catch {
                $why = $_.Exception.Message
                if ($why.StartsWith($Prefix)) { $why = $why.Substring($Prefix.Length) }
                Write-Host 'Uninstalling user memory...'
                Write-Left "what user-memory set up for Claude Code and Codex, the Docker memory server if there is one, and your memories and settings in $dataDir (kept): the installed user-memory is missing or does not run, and the latest release could not stand in for it ($why); run this again once this machine reaches GitHub, or install user-memory again with install.ps1 and then run this"
                Remove-Item -LiteralPath $state.Temporary -Recurse -Force -ErrorAction SilentlyContinue
                [Console]::Error.WriteLine("`nUninstall finished, but some items were left (above, each with what to do).")
                Exit-Run $state.Status
                return
            }
        }
        # The client side first, so no agent starts a session against a
        # half-removed install. This script removes the binary afterwards,
        # so it need not be named.
        $uninstall = @('uninstall', '--client', $clientName, '--install-dir', $installDirectory, '--binary-removed-after') + $choices
        # Then the Docker server the installer set up, through the binary,
        # which asks about its memories as `uninstall` does.
        $serverRemove = @()
        if (Test-Path -LiteralPath $compose) {
            $serverRemove = @('server', 'remove', '--dir', $dockerDir, '--name', $dockerName) + $choices
        }
        # The default install folders are this installer's own; another
        # folder given with -InstallDir is the user's.
        $emptyFolders = @($dataDir)
        if ($installDirectory -eq $defaultDirectory) {
            $emptyFolders = @($installDirectory, (Split-Path -Parent $installDirectory), $dataDir)
        }
        try {
            Remove-Installation -Tool $tool -Uninstall $uninstall -ServerRemove $serverRemove -DockerDir $dockerDir `
                -Binary $binary -EmptyFolders $emptyFolders
        } finally {
            if ($state.Temporary) { Remove-Item -LiteralPath $state.Temporary -Recurse -Force -ErrorAction SilentlyContinue }
        }
        if ($state.Status -ne 0) {
            [Console]::Error.WriteLine("`nUninstall finished, but some items were left (above, each with what to do).")
        }
        Exit-Run $state.Status
    }

    # Removes the client setup with TOOL and the UNINSTALL arguments, the
    # Docker server in DOCKERDIR with the SERVERREMOVE arguments (none when
    # there is no server), then BINARY, the copies an upgrade set aside
    # beside it, and EMPTYFOLDERS once nothing is left in them.
    function Remove-Installation {
        [Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSUseShouldProcessForStateChangingFunctions', '',
            Justification = 'An internal step of this script, whose -DryRun is its what-if.')]
        param(
            [string]$Tool,
            [string[]]$Uninstall,
            [string[]]$ServerRemove,
            [string]$DockerDir,
            [string]$Binary,
            [string[]]$EmptyFolders
        )
        # The binary asks its questions on this console.
        Invoke-Native { & $Tool @Uninstall }
        $uninstallStatus = $LASTEXITCODE
        # 1 means items were left and named; anything else, the binary
        # failed, and it is kept so a rerun can finish the job.
        $keepBinary = $false
        switch ($uninstallStatus) {
            0 { }
            1 { $state.Status = $ExitLeft }
            default {
                Write-Left 'the rest of the client setup: the installed user-memory failed (above); fix that, then run this again'
                $keepBinary = $true
            }
        }

        if ($ServerRemove) {
            Invoke-Native { & $Tool @ServerRemove }
            switch ($LASTEXITCODE) {
                0 { }
                1 { $state.Status = $ExitLeft }
                default {
                    Write-Left "the Docker memory server in ${DockerDir}: removing it failed (above); fix that, then run this again"
                    $keepBinary = $true
                }
            }
        }

        if ($keepBinary) { return }
        if (Test-Path -LiteralPath $Binary) { Remove-Binary $Binary }
        $folder = Split-Path -Parent $Binary
        foreach ($old in @(Get-ChildItem -LiteralPath $folder -Filter 'user-memory.old-*.exe' -File -ErrorAction SilentlyContinue)) {
            Remove-Binary $old.FullName
        }
        if (-not $DryRun) {
            foreach ($empty in $EmptyFolders) { Remove-EmptyFolder $empty }
        }
    }

    # GitHub needs TLS 1.2, which Windows PowerShell 5.1 may not offer by
    # default.
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    try {
        Invoke-Uninstall
    } catch {
        $message = $_.Exception.Message
        if ($message -notlike "$Prefix*") { $message = "$Prefix$message" }
        [Console]::Error.WriteLine($message)
        Exit-Run $ExitRefused
    }
}
