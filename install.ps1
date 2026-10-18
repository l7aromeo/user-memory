# Installs or upgrades the user-memory client on Windows from GitHub
# Releases and connects the Codex and Claude Code installations it finds to
# a memory server: an existing one, or one it sets up on this machine (in
# Docker Desktop or as a local store). The Windows twin of install.sh.
#
#   irm https://github.com/l7aromeo/user-memory/releases/latest/download/install.ps1 | iex
#   & ([scriptblock]::Create((irm https://github.com/l7aromeo/user-memory/releases/latest/download/install.ps1))) -Local docker
#
# Run it with -Help for the options. It runs on Windows PowerShell 5.1 and
# PowerShell 7. The questions, downloads and checksums are here; the
# downloaded user-memory.exe does the rest (setup, the Docker server, the
# embedding setting, private files).

# PSScriptAnalyzer: the parameters are read inside the child scope below,
# which the analyzer does not follow; and the script talks to the person at
# the console, so its messages go to the host (the information stream since
# PowerShell 5), never into the output that functions return.
[Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSReviewUnusedParameter', '',
    Justification = 'Read inside the child scope, which the analyzer does not follow.')]
[Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSAvoidUsingWriteHost', '',
    Justification = 'Console messages for the person running the installer; function output carries return values only.')]
param(
    [string]$Url,
    [string]$Local,
    [string]$Port,
    [string[]]$AllowedHost,
    [string]$Bind,
    [string]$TokenFile,
    [string]$Version,
    [string]$InstallDir,
    [string]$Client,
    [switch]$ServerOnly,
    [string]$Titles,
    [string]$EmbedUrl,
    [string]$EmbedModel,
    [string]$EmbedKeyFile,
    [string]$EmbedDimensions,
    [switch]$NoEmbed,
    [switch]$InstallOllama,
    [switch]$PullModel,
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

    # The recommended embedding model and its download size; `embedding set`
    # fills in the vector length and query instruction it is calibrated for.
    $EmbedModelDefault = 'qwen3-embedding:4b'
    $EmbedModelSize = 'about 2.5 GB'
    $OllamaDownload = 'https://ollama.com/download'
    # winget's package for Ollama (unverified until the Windows VM checks).
    $OllamaWingetId = 'Ollama.Ollama'
    # Seconds to wait for a newly installed or started Ollama to answer.
    $OllamaWait = 30
    # A release download that keeps failing still fails in seconds; a single
    # reset transfer (as GitHub's CDN sometimes does) is retried.
    $FetchRetries = 3
    $FetchRetryDelaySeconds = 1
    # Every access token user-memory writes is at least this long.
    $TokenMinimumLength = 32
    # The message of a failure whose cause is already printed above it (by
    # the binary, winget or doctor).
    $Reported = 'user-memory install: reported above'

    # What the run has decided so far, shared by the functions below.
    $state = @{}

    function Fail([string]$Message) {
        throw "user-memory install: $Message"
    }

    function Show-Usage {
        @'
Install or upgrade the user-memory client and connect it to a memory server.

Usage: & ([scriptblock]::Create((irm https://github.com/l7aromeo/user-memory/releases/latest/download/install.ps1))) [options]

  -Url URL               use an existing server, e.g. https://memory.example.com
                         (/mcp is added when missing)
  -Local docker|native   set up a server on this machine instead: a Docker
                         Desktop container on 127.0.0.1, or a local store
  -Port N                port of the local Docker server (default 8765)
  -AllowedHost HOST,...  for a Docker server shared through an HTTPS proxy:
                         accept requests for these hosts too (this machine's
                         own clients keep using 127.0.0.1)
  -Bind ADDR             also publish the Docker server on this IPv4 address,
                         for an HTTPS proxy on another machine (default:
                         127.0.0.1 only); never 0.0.0.0, as it speaks plain HTTP
  -TokenFile FILE        use this access-token file as it is
  -Version vX.Y.Z        install this release (default: the latest)
  -InstallDir DIR        where the binary goes
                         (default %LOCALAPPDATA%\Programs\user-memory\bin)
  -Client NAME           auto (default), codex, claude or both
  -ServerOnly            for a host with no agent app: run the server in Docker
                         (as -Local docker) and connect no client; kept by
                         reruns until -Client or -Url is given
  -Titles FILE           titles map for upgrading a store on this machine to
                         schema version 3

Semantic search, for a server set up on this machine (asked in a console;
a rerun keeps the earlier choice unless one of these is given):
  -EmbedModel NAME       use Ollama on this machine with this embedding model
                         (default qwen3-embedding:4b)
  -InstallOllama         install Ollama if it is missing, with winget
                         (accepting winget's source and package agreements)
  -PullModel             download the model into Ollama if it is missing
                         (qwen3-embedding:4b is about 2.5 GB)
  -EmbedUrl URL          use another OpenAI-compatible endpoint instead, with
                         -EmbedModel; /embeddings is added when missing
                         (/v1/embeddings for a bare host)
  -EmbedKeyFile FILE     API key for -EmbedUrl, read from FILE when used
  -EmbedDimensions N     ask the model for vectors of length N
  -NoEmbed               keyword search only

Without -Url or -Local, the installer reuses what an earlier install set
up, or asks. The server address can also come from USER_MEMORY_URL, and the
access token from USER_MEMORY_TOKEN; tokens are saved to
%LOCALAPPDATA%\user-memory\config\client-token, readable only by you, and
never printed. The binary is not code-signed yet: the installer checks its
SHA-256 against the release before running it.
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

    # Runs BINARY with ARGUMENTS and empty input, and fails with WHAT unless
    # it succeeds; without WHAT, the binary's own error stands.
    function Invoke-Binary([string]$Binary, [string[]]$Arguments, [string]$What) {
        Invoke-Native { $null | & $Binary @Arguments }
        if ($LASTEXITCODE -eq 0) { return }
        if ($What) { Fail $What }
        throw $Reported
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

    # Asks QUESTION; DEFAULT ('y' or 'n') answers an empty line.
    function Ask([string]$Question, [string]$Default) {
        $choices = '[y/N]'
        if ($Default -eq 'y') { $choices = '[Y/n]' }
        while ($true) {
            $answer = Read-Host "$Question $choices"
            switch -Regex ($answer) {
                '^$' { return ($Default -eq 'y') }
                '^(y|yes)$' { return $true }
                '^(n|no)$' { return $false }
            }
        }
    }

    # Asks for a number from 1 to COUNT; DEFAULT answers an empty line.
    function Choose([string]$Question, [int]$Count, [int]$Default) {
        while ($true) {
            $answer = Read-Host "$Question [$Default]"
            if ($answer -eq '') { return $Default }
            $number = 0
            if ([int]::TryParse($answer, [ref]$number) -and $number -ge 1 -and $number -le $Count) {
                return $number
            }
        }
    }

    # Reads a line without echoing it.
    function Read-Hidden([string]$Prompt) {
        $secure = Read-Host $Prompt -AsSecureString
        $pointer = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
        try {
            return [Runtime.InteropServices.Marshal]::PtrToStringBSTR($pointer)
        } finally {
            [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($pointer)
        }
    }

    function Get-AbsolutePath([string]$Path) {
        return $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Path)
    }

    # Checks an http(s)://host[:port][/path] address and returns it without
    # space or trailing slashes.
    function Get-CleanUrl([string]$Value) {
        $clean = ($Value -replace '\s', '').TrimEnd('/')
        if ($clean -notmatch '^https?://([A-Za-z0-9.-]+|\[[0-9A-Fa-f:.]+\])(:[0-9]{1,5})?(/[A-Za-z0-9._~/-]*)?$') {
            Fail "not an address: $Value (expected http(s)://host[:port][/path])"
        }
        return $clean
    }

    # The MCP endpoint of a server address: /mcp appended unless the path
    # already ends in it.
    function Get-Endpoint([string]$Value) {
        $clean = Get-CleanUrl $Value
        if ($clean -match '/mcp$') { return $clean }
        return "$clean/mcp"
    }

    # Whether VALUE is plain http to this machine.
    function Test-LoopbackHttp([string]$Value) {
        return ($Value -match '^http://(127\.0\.0\.1|localhost|\[::1\])(:[0-9]+)?(/|$)')
    }

    # Fails unless VALUE is https, or plain http to this machine: tokens and
    # binaries never travel unencrypted across a network.
    function Assert-Secure([string]$Value, [string]$What) {
        if ($Value -match '^https://') { return }
        if (-not (Test-LoopbackHttp $Value)) {
            Fail "$What $Value uses plain HTTP; use https:// (plain http is allowed only for this machine)"
        }
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
                Write-Host "Retrying the download ($attempt of $FetchRetries)"
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

    # Downloads the release's NAME to FILE and checks its SHA-256 against
    # NAME.sha256; nothing is installed from a file that does not match.
    function Get-Verified([string]$Name, [string]$File) {
        if (-not (Get-Download "$($state.Assets)/$Name" $File)) { Fail "cannot download $($state.Assets)/$Name" }
        if (-not (Get-Download "$($state.Assets)/$Name.sha256" "$File.sha256")) {
            Fail "cannot download $($state.Assets)/$Name.sha256"
        }
        $expected = ([string](Get-Content -LiteralPath "$File.sha256" -TotalCount 1 -ErrorAction Stop) -split '\s+')[0]
        if ($expected -notmatch '^[0-9a-f]{64}$') {
            Fail "the release's checksum file is malformed; nothing was installed"
        }
        if ((Get-Sha256 $File) -ne $expected) {
            Fail 'the download does not match its checksum; nothing was installed'
        }
    }

    # Makes FILE readable only by this user, through the downloaded binary,
    # so this script holds no access-control code.
    function Protect-File([string]$File) {
        Invoke-Binary -Binary $state.Downloaded -Arguments @('secure-file', $File) -What "cannot make $File private"
    }

    # Saves TEXT to FILE, readable only by this user, replaced in one rename.
    function Save-Secret([string]$Text, [string]$File) {
        New-Item -ItemType Directory -Force -Path (Split-Path -Parent $File) -ErrorAction Stop | Out-Null
        $temporary = "$File.$PID"
        [IO.File]::WriteAllText($temporary, "$Text`n")
        Protect-File $temporary
        Move-Item -LiteralPath $temporary -Destination $File -Force -ErrorAction Stop
    }

    function Save-Token([string]$Token) {
        if ($Token.Length -lt $TokenMinimumLength) {
            Fail "that is not an access token (it has fewer than $TokenMinimumLength characters)"
        }
        Save-Secret $Token $state.TokenPath
    }

    function Read-Token {
        if (-not (Test-Interactive)) { Fail 'no access token: set USER_MEMORY_TOKEN or pass -TokenFile' }
        Save-Token (Read-Hidden "Access token for $($state.Endpoint) (input hidden)")
    }

    function Test-DockerReady {
        if (-not (Get-Command docker -ErrorAction SilentlyContinue)) { return $false }
        Invoke-Native { & docker info *> $null }
        return ($LASTEXITCODE -eq 0)
    }

    # The server address and token file an earlier install saved, as the
    # installed BINARY reads them with the environment's overrides hidden;
    # nothing when there is no such setup.
    function Get-SavedEndpoint([string]$Binary) {
        $hidden = @{}
        foreach ($name in @('USER_MEMORY_URL', 'USER_MEMORY_TOKEN_FILE', 'USER_MEMORY_DB')) {
            $hidden[$name] = [Environment]::GetEnvironmentVariable($name)
            [Environment]::SetEnvironmentVariable($name, $null)
        }
        try {
            $found = @(Invoke-Native { & $Binary endpoint 2>$null })
            if ($LASTEXITCODE -eq 0 -and $found.Count -ge 2) { return $found }
            return @()
        } finally {
            foreach ($name in $hidden.Keys) { [Environment]::SetEnvironmentVariable($name, $hidden[$name]) }
        }
    }

    # Whether Ollama answers at its address.
    function Test-OllamaRunning {
        try {
            Invoke-RestMethod -Uri "$($state.Ollama)/api/tags" -TimeoutSec 3 -UseBasicParsing -ErrorAction Stop | Out-Null
            return $true
        } catch {
            return $false
        }
    }

    # The Ollama command: on PATH, or where Ollama's Windows installer puts
    # it (a fresh install's PATH reaches only new terminals).
    function Get-OllamaCommand {
        $found = Get-Command ollama -ErrorAction SilentlyContinue
        if ($found) { return $found.Source }
        $installed = Join-Path $env:LOCALAPPDATA 'Programs\Ollama\ollama.exe'
        if (Test-Path -LiteralPath $installed) { return $installed }
        return $null
    }

    # The models Ollama has pulled that make embeddings: those whose
    # capabilities name embedding, or, from an Ollama too old to report
    # capabilities, whose name says embed.
    function Get-EmbeddingModel {
        $names = @()
        try {
            $tags = Invoke-RestMethod -Uri "$($state.Ollama)/api/tags" -TimeoutSec 5 -UseBasicParsing -ErrorAction Stop
        } catch {
            return $names
        }
        if ($tags.PSObject.Properties.Name -notcontains 'models') { return $names }
        foreach ($model in @($tags.models)) {
            $name = [string]$model.name
            try {
                $shown = Invoke-RestMethod -Uri "$($state.Ollama)/api/show" -Method Post -TimeoutSec 5 -UseBasicParsing -ErrorAction Stop `
                    -ContentType 'application/json' -Body (@{ model = $name } | ConvertTo-Json -Compress)
            } catch {
                continue
            }
            if ($shown.PSObject.Properties.Name -contains 'capabilities') {
                if (@($shown.capabilities) -contains 'embedding') { $names += $name }
            } elseif ($name -like '*embed*') {
                $names += $name
            }
        }
        return $names
    }

    # Waits up to OllamaWait seconds for Ollama to answer.
    function Wait-Ollama {
        for ($waited = 0; $waited -lt $OllamaWait; $waited++) {
            if (Test-OllamaRunning) { return $true }
            Start-Sleep -Seconds 1
        }
        return (Test-OllamaRunning)
    }

    # Installs Ollama with winget, only once the user has said yes: an
    # answer, after which winget asks for its own agreements, or
    # -InstallOllama, which accepts them.
    function Install-Ollama([bool]$Answered) {
        if (-not (Get-Command winget -ErrorAction SilentlyContinue)) {
            Fail "winget is not available, so Ollama cannot be installed here; install it from $OllamaDownload, then run the installer again"
        }
        Write-Host "Installing Ollama with winget ($OllamaWingetId)"
        $arguments = @('install', '--exact', '--id', $OllamaWingetId)
        if (-not $Answered) { $arguments += @('--accept-source-agreements', '--accept-package-agreements') }
        Invoke-Native { & winget @arguments }
        if ($LASTEXITCODE -ne 0) { Fail 'winget install failed (above)' }
    }

    # Downloads MODEL into Ollama, with the Ollama command's progress when it
    # is there.
    function Get-OllamaModel([string]$Model) {
        if ($Model -eq $EmbedModelDefault) {
            Write-Host "Downloading $Model ($EmbedModelSize)"
        } else {
            Write-Host "Downloading $Model"
        }
        $command = Get-OllamaCommand
        if ($command) {
            $saved = $env:OLLAMA_HOST
            $env:OLLAMA_HOST = $state.Ollama
            try { Invoke-Native { & $command pull $Model } } finally { $env:OLLAMA_HOST = $saved }
            if ($LASTEXITCODE -ne 0) { Fail "ollama pull $Model failed (above)" }
        } else {
            try {
                Invoke-RestMethod -Uri "$($state.Ollama)/api/pull" -Method Post -UseBasicParsing -ErrorAction Stop `
                    -ContentType 'application/json' -Body (@{ model = $Model; stream = $false } | ConvertTo-Json -Compress) | Out-Null
            } catch {
                Fail "Ollama could not download $Model"
            }
        }
    }

    # Chooses Ollama on this machine and a model it has, installing Ollama
    # or downloading the model only with the user's yes (ASK) or its option.
    # Sets Embed to set, or to off when the user declines.
    function Use-Ollama([bool]$Ask) {
        $state.Embed = 'off'
        if (-not (Test-OllamaRunning)) {
            if (Get-OllamaCommand) {
                if (-not $Ask) { Fail "Ollama is installed but not running at $($state.Ollama); start it, then run the installer again" }
                Read-Host "Ollama is installed but not running at $($state.Ollama). Start it from the Start menu, then press Enter" | Out-Null
            } elseif ($InstallOllama) {
                Install-Ollama $false
            } elseif ($Ask) {
                if (-not (Ask "Ollama isn't installed. Install it now?" 'n')) { return }
                Install-Ollama $true
            } else {
                Fail "Ollama isn't installed; pass -InstallOllama to install it, or install it from $OllamaDownload"
            }
            if (-not (Wait-Ollama)) { Fail "Ollama does not answer at $($state.Ollama); start it, then run the installer again" }
        }
        $models = @(Get-EmbeddingModel)
        $name = $EmbedModel
        $chosenDownload = $false
        # Other embedding models Ollama has, offered beside the recommended one.
        $listed = @($models | Where-Object { $_ -ne $EmbedModelDefault })
        if (-not $name -and $Ask -and $listed.Count -gt 0) {
            $first = "download $EmbedModelDefault ($EmbedModelSize, recommended)"
            if ($models -contains $EmbedModelDefault) { $first = "$EmbedModelDefault (recommended)" }
            Write-Host 'Embedding models:'
            Write-Host "  1) $first"
            for ($i = 0; $i -lt $listed.Count; $i++) { Write-Host "  $($i + 2)) $($listed[$i])" }
            $picked = Choose -Question 'Model' -Count ($listed.Count + 1) -Default 1
            if ($picked -eq 1) {
                $name = $EmbedModelDefault
                $chosenDownload = $true
            } else {
                $name = $listed[$picked - 2]
            }
        }
        if (-not $name) { $name = $EmbedModelDefault }
        if ($models -notcontains $name) {
            if ($PullModel -or $chosenDownload) {
                Get-OllamaModel $name
            } elseif ($Ask) {
                $question = "Download ${name}?"
                if ($name -eq $EmbedModelDefault) { $question = "Download $name ($EmbedModelSize)?" }
                if (-not (Ask $question 'y')) { return }
                Get-OllamaModel $name
            } else {
                Fail "Ollama has no model $name; pass -PullModel to download it, or run: ollama pull $name"
            }
        }
        $state.Embed = 'set'
        $state.EmbedUrl = $state.Ollama
        $state.EmbedName = $name
    }

    # An embeddings address as given, checked to be HTTPS or plain http to
    # this machine; `embedding set` adds the endpoint's path.
    function Get-EmbedAddress([string]$Value) {
        $clean = Get-CleanUrl $Value
        Assert-Secure $clean 'the embeddings endpoint'
        return $clean
    }

    # Asks for another OpenAI-compatible endpoint: its address, the model and
    # an API key, read hidden and saved privately.
    function Read-EmbedEndpoint {
        $answer = Read-Host 'Embeddings address (e.g. http://host:11434 or https://api.openai.com/v1)'
        if (-not $answer) { Fail 'no embeddings address given' }
        $state.EmbedUrl = Get-EmbedAddress $answer
        $state.EmbedName = Read-Host 'Model name'
        if (-not $state.EmbedName) { Fail 'no model name given' }
        $key = Read-Hidden 'API key (input hidden; Enter for none)'
        if ($key) {
            Save-Secret $key $state.EmbedKeyPath
            $state.EmbedKey = $state.EmbedKeyPath
        }
        $state.Embed = 'set'
    }

    # Decides the embedding setting for a store on this machine: Embed is
    # keep (a rerun without embedding options), set, or off. Nothing is
    # installed or downloaded without the user's yes.
    function Select-Embedding {
        $state.Embed = 'keep'
        $state.EmbedUrl = ''
        $state.EmbedName = ''
        $state.EmbedKey = $state.EmbedKeyFile
        if ($NoEmbed) {
            $state.Embed = 'off'
        } elseif ($state.EmbedUrlGiven) {
            $state.Embed = 'set'
            $state.EmbedUrl = $state.EmbedUrlGiven
            $state.EmbedName = $EmbedModel
        } elseif ($state.OllamaGiven) {
            Use-Ollama $false
        } elseif ($state.Upgrade -or $state.Saved -eq 'native') {
            return
        } elseif (Test-Interactive) {
            Write-Host 'Semantic search finds memories by meaning as well as by their words. It needs an embedding model:'
            Write-Host '  1) Ollama on this machine (recommended: private, no API key)'
            Write-Host '  2) Another OpenAI-compatible endpoint'
            Write-Host '  3) Off: keyword search only'
            switch (Choose -Question 'Semantic search' -Count 3 -Default 1) {
                1 { Use-Ollama $true }
                2 { Read-EmbedEndpoint }
                3 { $state.Embed = 'off' }
            }
        } elseif ((Test-OllamaRunning) -and (@(Get-EmbeddingModel) -contains $EmbedModelDefault)) {
            Write-Host "Ollama has ${EmbedModelDefault}: using it for semantic search."
            $state.Embed = 'set'
            $state.EmbedUrl = $state.Ollama
            $state.EmbedName = $EmbedModelDefault
        } else {
            $state.Embed = 'off'
        }
        if ($state.Embed -eq 'off') {
            Write-Host "Semantic search is off (keyword search only). To turn it on later, run this installer again with -EmbedModel $EmbedModelDefault -PullModel, or with -EmbedUrl URL -EmbedModel NAME."
        }
    }

    # Applies the choice with the installed binary, which checks the
    # endpoint, calibrates the model, saves the setting where the store lives
    # and embeds the memories again (for the Docker server, inside its
    # container). A store without a setting has none to turn off.
    function Set-Embedding {
        [Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSUseShouldProcessForStateChangingFunctions', '',
            Justification = 'An internal step of this script, which has no what-if: it only applies what the user chose.')]
        param()
        switch ($state.Embed) {
            'set' {
                $arguments = @('embedding', 'set', '--url', $state.EmbedUrl, '--model', $state.EmbedName)
                if ($state.EmbedKey) { $arguments += @('--key-file', $state.EmbedKey) }
                if ($EmbedDimensions) { $arguments += @('--dimensions', $EmbedDimensions) }
            }
            'off' {
                if (-not ($state.Upgrade -or $state.Saved -eq 'native')) { return }
                $arguments = @('embedding', 'off')
            }
            default { return }
        }
        Invoke-Binary -Binary $state.Installed -Arguments $arguments -What "semantic search was not changed (above), though everything else is installed; once that is fixed, run: & '$($state.Installed)' $($arguments -join ' ')"
    }

    # Sets up or upgrades the Docker server through the downloaded binary,
    # which writes the project, builds it, upgrades the store and copies the
    # token; this script asks the questions and downloads the builds.
    function Install-DockerServer {
        $place = @('--dir', $state.DockerDir, '--name', $state.DockerName, '--port', $state.Port)
        if ($state.Upgrade) { $place += '--upgrade' }
        # Before any question: never take over another server's port or memories.
        Invoke-Binary -Binary $state.Downloaded -Arguments (@('server', 'check') + $place)
        Select-Embedding
        $serverTarget = [string](Invoke-Native { & $state.Downloaded server target })
        if ($LASTEXITCODE -ne 0) { throw $Reported }
        Write-Host "Downloading the $serverTarget server build"
        $build = Join-Path $state.Temporary 'server'
        Get-Verified "user-memory-$serverTarget" $build
        $arguments = @('server', 'install') + $place + @('--build', $build)
        # An upgrade keeps the shared-server choices it was not given again.
        foreach ($name in $state.Hosts) { $arguments += @('--allowed-host', $name) }
        if ($Bind) { $arguments += @('--bind', $Bind) }
        if ($state.Titles) { $arguments += @('--titles', $state.Titles) }
        $state.Endpoint = "http://127.0.0.1:$($state.Port)/mcp"
        $state.TokenUsed = $state.TokenPath
        if ($state.Upgrade -and $state.SavedToken -and (Test-Path -LiteralPath $state.SavedToken)) {
            $state.TokenUsed = $state.SavedToken
        } else {
            $arguments += @('--token-file', $state.TokenPath)
        }
        # The store check may ask about the titles map, so the server
        # command keeps this console's input.
        Invoke-Native { & $state.Downloaded @arguments }
        if ($LASTEXITCODE -ne 0) { throw $Reported }
        # What the server now accepts, for the closing summary.
        $compose = @(Get-Content -LiteralPath (Join-Path $state.DockerDir 'compose.yaml') -ErrorAction Stop)
        $state.Hosts = @($compose | ForEach-Object { if ($_ -match '^ *- --allowed-host=(.*)$') { $Matches[1] } })
        $state.Bind = '127.0.0.1'
        foreach ($line in $compose) {
            if ($line -match '^ *- "([0-9.]*):[0-9]*:8765"$' -and $Matches[1] -ne '127.0.0.1') {
                $state.Bind = $Matches[1]
                break
            }
        }
    }

    # Prints the closing summary from doctor's output DOCTOR and its status,
    # and fails when doctor found a problem. Everything is installed whatever
    # doctor finds, so how to use it is said either way. Status 3 means
    # nothing is broken but the user has a step left, which the run repeats
    # last and does not fail on.
    function Show-Summary([string]$LocalMode, [bool]$ServerOnlyHost, [string]$Releases, [object[]]$Doctor, [int]$DoctorStatus) {
        if ($LocalMode -eq 'docker') {
            $compose = Join-Path $state.DockerDir 'compose.yaml'
            Write-Host ''
            Write-Host "The memory server runs in Docker on 127.0.0.1:$($state.Port), from $($state.DockerDir)."
            Write-Host "  Stop or start it:  docker compose -f `"$compose`" stop   (or start)"
            Write-Host "  Logs:              docker compose -f `"$compose`" logs memory"
            Write-Host '  Upgrade:           run this installer again; it rebuilds the server from the'
            Write-Host '                     newer release and upgrades the store, backing it up first.'
            Write-Host "Its memories live in the Docker volume $($state.DockerName)-data."
            if ($state.Hosts.Count -gt 0) {
                Write-Host ''
                Write-Host "Other machines reach it through your HTTPS proxy, which forwards to http://$($state.Bind):$($state.Port), at:"
                foreach ($name in $state.Hosts) { Write-Host "  https://$name" }
                Write-Host "On each of them, save this machine's access token (from $($state.TokenUsed)) in a"
                Write-Host 'file only you can read, then run, on macOS or Linux:'
                Write-Host "  curl -fsSL $Releases/latest/download/install.sh | sh -s -- --url https://$($state.Hosts[0]) --token-file /path/to/token"
                Write-Host 'or on Windows:'
                Write-Host "  & ([scriptblock]::Create((irm $Releases/latest/download/install.ps1))) -Url https://$($state.Hosts[0]) -TokenFile C:\path\to\token"
            }
            if ($ServerOnlyHost) {
                Write-Host ''
                Write-Host 'This machine runs only the memory server; no agent app here uses it. Its access'
                Write-Host "token is in $($state.TokenUsed); every machine that uses the server needs a copy."
                if ($state.Hosts.Count -eq 0) {
                    Write-Host 'Other machines reach it through an HTTPS proxy: put one in front of it, then run'
                    Write-Host 'this installer again with -AllowedHost HOST (and -Bind ADDR for a proxy on'
                    Write-Host 'another machine) to print their install command.'
                }
            }
        }
        $installDirectory = Split-Path -Parent $state.Installed
        if (@($env:Path -split ';') -notcontains $installDirectory) {
            Write-Host ''
            Write-Host "Open a new terminal to run user-memory from a shell ($installDirectory is on your user PATH from now on)."
        }
        $steps = @($Doctor | Where-Object { $_ -like 'Action needed: *' })
        if ($steps.Count -gt 0) {
            Write-Host ''
            Write-Host 'Still to do before memory works everywhere:'
            foreach ($step in $steps) { Write-Host ('  ' + $step.Substring('Action needed: '.Length)) }
        }
        if ($DoctorStatus -ne 0 -and $DoctorStatus -ne 3) {
            [Console]::Error.WriteLine("`nThe install finished, but doctor found a problem (above); fix it, then run & '$($state.Installed)' doctor again.")
            throw $Reported
        }
    }

    function Invoke-Install {
        if ($Help) {
            Show-Usage
            return
        }

        $installDirectory = $InstallDir
        if (-not $installDirectory) { $installDirectory = Join-Path $env:LOCALAPPDATA 'Programs\user-memory\bin' }
        $installDirectory = Get-AbsolutePath $installDirectory
        $state.Installed = Join-Path $installDirectory 'user-memory.exe'
        $clientName = $Client
        if (-not $clientName) { $clientName = 'auto' }
        $serverOnlyHost = [bool]$ServerOnly
        $localMode = $Local
        $serverUrl = $Url
        $state.Titles = $null
        if ($Titles) { $state.Titles = Get-AbsolutePath $Titles }
        $state.EmbedKeyFile = $null
        if ($EmbedKeyFile) { $state.EmbedKeyFile = Get-AbsolutePath $EmbedKeyFile }

        if ($localMode -and $localMode -notin @('docker', 'native')) { Fail '-Local takes docker or native' }
        if ($clientName -notin @('auto', 'codex', 'claude', 'both')) { Fail '-Client takes auto, codex, claude or both' }
        if ($state.Titles -and -not (Test-Path -LiteralPath $state.Titles)) { Fail "cannot read the titles map $($state.Titles)" }
        if ($localMode -and $serverUrl) { Fail 'give either -Url or -Local, not both' }
        if ($serverOnlyHost) {
            if ($serverUrl) { Fail '-ServerOnly runs the memory server on this machine, while -Url connects to one elsewhere; give one of them' }
            if ($localMode -eq 'native') { Fail '-ServerOnly runs the server in Docker; leave out -Local native' }
            if ($Client) { Fail "-ServerOnly connects no agent app; leave out -Client, or use -Local docker to connect this machine's agents to the server too" }
        }
        $state.Hosts = @()
        foreach ($name in @($AllowedHost)) {
            if (-not $name) { continue }
            if ($name -notmatch '^[A-Za-z0-9]([A-Za-z0-9.-]*[A-Za-z0-9])?$') {
                Fail '-AllowedHost takes a host name such as memory.example.com, without https:// or a path'
            }
            $state.Hosts += $name.ToLowerInvariant()
        }
        if ($Bind) {
            if ($Bind -in @('0.0.0.0', '::', '[::]', '*')) {
                Fail "-Bind $Bind would serve plain HTTP on every network; keep 127.0.0.1 for an HTTPS proxy on this machine, or bind a private or tunnel address that only the proxy reaches"
            }
            if ($Bind -notmatch '^[0-9]{1,3}(\.[0-9]{1,3}){3}$') { Fail '-Bind takes an IPv4 address such as 127.0.0.1 or 10.0.0.5' }
        }
        $state.Port = '8765'
        if ($Port) {
            $number = 0
            if ($Port -notmatch '^[0-9]{1,5}$' -or -not [int]::TryParse($Port, [ref]$number) -or $number -lt 1 -or $number -gt 65535) {
                Fail '-Port takes a port number'
            }
            $state.Port = [string]$number
        }
        if ($Version -and $Version -notmatch '^v[0-9][A-Za-z0-9._-]*$') { Fail '-Version takes a release tag such as v0.3.0' }

        $embedGiven = [bool]($EmbedUrl -or $EmbedModel -or $EmbedKeyFile -or $EmbedDimensions -or $NoEmbed -or $InstallOllama -or $PullModel)
        $state.OllamaGiven = $false
        $state.EmbedUrlGiven = $null
        if ($NoEmbed) {
            if ($EmbedUrl -or $EmbedModel -or $EmbedKeyFile -or $EmbedDimensions -or $InstallOllama -or $PullModel) {
                Fail '-NoEmbed goes without the other semantic search options'
            }
        } elseif ($EmbedUrl) {
            if (-not $EmbedModel) { Fail '-EmbedUrl needs -EmbedModel' }
            if ($InstallOllama -or $PullModel) { Fail '-InstallOllama and -PullModel are for Ollama on this machine, not -EmbedUrl' }
            $state.EmbedUrlGiven = Get-EmbedAddress $EmbedUrl
        } elseif ($EmbedKeyFile) {
            Fail '-EmbedKeyFile goes with -EmbedUrl'
        } elseif ($embedGiven) {
            $state.OllamaGiven = $true
        }
        if ($EmbedDimensions -and $EmbedDimensions -notmatch '^[1-9][0-9]*$') { Fail '-EmbedDimensions takes a positive whole number' }
        if ($state.EmbedKeyFile -and -not (Test-Path -LiteralPath $state.EmbedKeyFile)) {
            Fail "cannot read the API key file $($state.EmbedKeyFile)"
        }

        $releasesBase = $env:USER_MEMORY_RELEASES
        if (-not $releasesBase) { $releasesBase = 'https://github.com/l7aromeo/user-memory/releases' }
        $releases = Get-CleanUrl $releasesBase
        Assert-Secure $releases 'the releases address'
        $ollamaBase = $env:USER_MEMORY_OLLAMA_URL
        if (-not $ollamaBase) { $ollamaBase = 'http://127.0.0.1:11434' }
        $state.Ollama = Get-CleanUrl $ollamaBase
        if (-not (Test-LoopbackHttp $state.Ollama)) { Fail 'USER_MEMORY_OLLAMA_URL must be plain http on this machine' }
        $configDir = Join-Path $env:LOCALAPPDATA 'user-memory\config'
        $state.TokenPath = Join-Path $configDir 'client-token'
        # Where an API key typed for another embeddings endpoint is saved.
        $state.EmbedKeyPath = Join-Path $configDir 'embedding-key'
        $state.DockerName = $env:USER_MEMORY_DOCKER_NAME
        if (-not $state.DockerName) { $state.DockerName = 'user-memory' }
        if ($state.DockerName -notmatch '^[a-z0-9][a-z0-9_-]*$') { Fail 'USER_MEMORY_DOCKER_NAME must be a lowercase Docker project name' }
        $dockerFolder = 'docker'
        if ($state.DockerName -ne 'user-memory') { $dockerFolder = "docker-$($state.DockerName)" }
        $state.DockerDir = Join-Path $env:LOCALAPPDATA "user-memory\$dockerFolder"

        # What an earlier install set up: a shared server (possibly the Docker
        # one this installer runs), or a local store.
        $savedUrl = $null
        $state.SavedToken = $null
        $state.Saved = 'none'
        $dockerPort = $null
        $receipt = Join-Path $installDirectory '.user-memory.json'
        if (Test-Path -LiteralPath $state.Installed) {
            $found = @(Get-SavedEndpoint $state.Installed)
            if ($found.Count -ge 2) {
                $savedUrl = [string]$found[0]
                $state.SavedToken = [string]$found[1]
                $state.Saved = 'remote'
                $compose = Join-Path $state.DockerDir 'compose.yaml'
                if (Test-Path -LiteralPath $compose) {
                    foreach ($line in Get-Content -LiteralPath $compose) {
                        if ($line -match '^ *- "127\.0\.0\.1:([0-9]*):8765"$') {
                            $dockerPort = $Matches[1]
                            break
                        }
                    }
                    if ($dockerPort -and $savedUrl -eq "http://127.0.0.1:$dockerPort/mcp") { $state.Saved = 'docker' }
                }
            } elseif (Test-Path -LiteralPath $receipt) {
                $state.Saved = 'native'
            }
        }
        # A server-only host stays one until -Client, -Url or -Local native
        # makes it a client install.
        if ($state.Saved -eq 'docker' -and -not $serverUrl -and $localMode -ne 'native' -and -not $Client -and
            (Test-Path -LiteralPath $receipt) -and (Select-String -LiteralPath $receipt -Pattern '"server_only": true' -SimpleMatch -Quiet)) {
            $serverOnlyHost = $true
        }
        if ($serverOnlyHost -and -not $localMode) { $localMode = 'docker' }

        # Where the memory lives: flag, environment, earlier install, then ask.
        if (-not $serverUrl -and -not $localMode) { $serverUrl = $env:USER_MEMORY_URL }
        $state.Upgrade = $false
        if (-not $serverUrl -and -not $localMode) {
            switch ($state.Saved) {
                'remote' {
                    $serverUrl = $savedUrl
                    Write-Host "Memory server: $serverUrl (from the earlier install)"
                }
                'docker' {
                    $localMode = 'docker'
                    Write-Host "Upgrading the Docker memory server in $($state.DockerDir)"
                }
                'native' {
                    $localMode = 'native'
                    Write-Host 'Upgrading the local memory store install'
                }
            }
        }
        if (-not $serverUrl -and -not $localMode) {
            if (-not (Test-Interactive)) { Fail 'no server address: pass -Url, or -Local docker|native' }
            if (Ask 'Do you already have a user-memory server?' 'n') {
                $serverUrl = Read-Host 'Memory server address (e.g. https://memory.example.com or http://127.0.0.1:8765)'
                if (-not $serverUrl) { Fail 'no server address given' }
            } elseif (-not (Ask 'Set up a memory server on this machine?' 'y')) {
                Write-Host 'Nothing was installed. Run this installer again with -Url URL once you have a server, or with -Local docker|native to set one up here.'
                return
            } elseif (Test-DockerReady) {
                $localMode = 'native'
                if (Ask 'Run it in Docker?' 'y') { $localMode = 'docker' }
            } else {
                Write-Host 'Docker is not available here (no docker command, or docker info failed).'
                if (-not (Ask 'Keep the memories in a local store on this machine instead?' 'y')) {
                    Write-Host 'Nothing was installed. Start Docker Desktop and run this installer again, or pass -Local native.'
                    return
                }
                $localMode = 'native'
            }
        }
        if (-not $localMode) {
            $state.Endpoint = Get-Endpoint $serverUrl
            Assert-Secure $state.Endpoint 'the memory server'
            if ($embedGiven) { Fail 'semantic search is set where the memory server runs; pass these options to the installer on that machine' }
        }
        if ($localMode -eq 'docker' -and $state.Saved -eq 'docker') {
            # The Docker server this installer set up earlier: upgrade it in place.
            $state.Upgrade = $true
            if (-not $Port) { $state.Port = $dockerPort }
        }
        if ($localMode -ne 'docker' -and ($state.Hosts.Count -gt 0 -or $Bind)) {
            Fail '-AllowedHost and -Bind set up a shared Docker server; use them with -Local docker'
        }

        $state.Assets = "$releases/latest/download"
        if ($Version) { $state.Assets = "$releases/download/$Version" }
        $target = Get-Target
        $state.Temporary = Join-Path ([IO.Path]::GetTempPath()) ('user-memory-install-' + [Guid]::NewGuid().ToString('N'))
        New-Item -ItemType Directory -Path $state.Temporary -ErrorAction Stop | Out-Null
        try {
            $shown = $Version
            if (-not $shown) { $shown = '(latest)' }
            Write-Host "Downloading user-memory $shown for $target"
            $state.Downloaded = Join-Path $state.Temporary 'user-memory.exe'
            Get-Verified "user-memory-$target.exe" $state.Downloaded
            Write-Host 'Checksum: OK'
            # The binary is not code-signed yet, so Windows may refuse to
            # start it; that is named here rather than as a failed step.
            if (-not (Test-Runnable $state.Downloaded)) {
                Fail "Windows did not run the downloaded user-memory.exe. It is not code-signed yet, so Smart App Control or an antivirus may block it. Its SHA-256 matched the release; to check that yourself, download user-memory-$target.exe from $($state.Assets) and compare (Get-FileHash user-memory-$target.exe).Hash with user-memory-$target.exe.sha256"
            }

            # Setup installs the verified client into the install folder,
            # connects the clients (none on a server-only host), adds the
            # folder to the user PATH and checks the server; doctor confirms
            # the result.
            switch ($localMode) {
                'docker' {
                    Install-DockerServer
                    $arguments = @('setup', '--url', $state.Endpoint, '--token-file', $state.TokenUsed, '--install-dir', $installDirectory)
                    if ($serverOnlyHost) { $arguments += '--server-only' } else { $arguments += @('--client', $clientName) }
                    Invoke-Binary -Binary $state.Downloaded -Arguments $arguments
                    Set-Embedding
                }
                'native' {
                    Select-Embedding
                    $arguments = @('setup', '--install-dir', $installDirectory, '--client', $clientName)
                    if ($state.Titles) { $arguments += @('--titles', $state.Titles) }
                    Invoke-Binary -Binary $state.Downloaded -Arguments $arguments
                    Set-Embedding
                }
                default {
                    if ($state.Titles) {
                        Fail "-Titles upgrades a store on this machine; a remote server's store is upgraded on the server"
                    }
                    if ($TokenFile) {
                        $state.TokenUsed = Get-AbsolutePath $TokenFile
                        if (-not (Test-Path -LiteralPath $state.TokenUsed)) { Fail "cannot read the token file $($state.TokenUsed)" }
                    } else {
                        $state.TokenUsed = $state.TokenPath
                        if ($env:USER_MEMORY_TOKEN) {
                            Save-Token $env:USER_MEMORY_TOKEN
                        } elseif ($state.Endpoint -eq $savedUrl -and $state.SavedToken -and (Test-Path -LiteralPath $state.SavedToken)) {
                            # Re-running for the same server keeps the token it already uses.
                            $state.TokenUsed = $state.SavedToken
                        } else {
                            Read-Token
                        }
                    }
                    $arguments = @('setup', '--url', $state.Endpoint, '--token-file', $state.TokenUsed, '--install-dir', $installDirectory, '--client', $clientName)
                    Invoke-Binary -Binary $state.Downloaded -Arguments $arguments
                }
            }

            # The release just installed is the newest one there is to check for.
            $saved = $env:USER_MEMORY_NO_UPDATE_CHECK
            $env:USER_MEMORY_NO_UPDATE_CHECK = '1'
            try {
                $doctor = @(Invoke-Native { $null | & $state.Installed doctor })
                $doctorStatus = $LASTEXITCODE
            } finally {
                $env:USER_MEMORY_NO_UPDATE_CHECK = $saved
            }
            foreach ($line in $doctor) { Write-Host $line }
            Show-Summary -LocalMode $localMode -ServerOnlyHost $serverOnlyHost -Releases $releases -Doctor $doctor -DoctorStatus $doctorStatus
        } finally {
            Remove-Item -LiteralPath $state.Temporary -Recurse -Force -ErrorAction SilentlyContinue
        }
    }

    # GitHub needs TLS 1.2, which Windows PowerShell 5.1 may not offer by
    # default.
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    try {
        Invoke-Install
    } catch {
        $message = $_.Exception.Message
        if ($message -ne $Reported) {
            if ($message -notlike 'user-memory install: *') { $message = "user-memory install: $message" }
            [Console]::Error.WriteLine($message)
        }
        # Pasted into a console, `exit` would close it: the run ends with the
        # failure's exit code set and the console left open.
        if (-not (Test-MayExit)) {
            $global:LASTEXITCODE = 1
            return
        }
        exit 1
    }
}
