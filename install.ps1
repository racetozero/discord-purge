[CmdletBinding()]
param(
    [string]$Version = $env:DISCORD_PURGE_VERSION,
    [string]$Target = $env:DISCORD_PURGE_TARGET,
    [string]$InstallDirectory = $env:DISCORD_PURGE_INSTALL_DIR,
    [string]$Repository = $env:DISCORD_PURGE_REPOSITORY,
    [string]$ReleasesUrl = $env:DISCORD_PURGE_RELEASES_URL
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version 3.0

function Fail([string]$Message) {
    throw "discord-purge installer: $Message"
}

function Normalize-Tag([string]$Requested) {
    $Normalized = if ($Requested.StartsWith("v")) { $Requested } else { "v$Requested" }
    $PlainVersion = $Normalized.Substring(1)
    if (-not $PlainVersion -or $PlainVersion -notmatch '^[0-9A-Za-z.+-]+$') {
        Fail "the version is '$Requested'. Use latest or a release version such as 0.1.0 or v0.1.0"
    }
    return @($Normalized, $PlainVersion)
}

function Test-GitHubCli {
    if ($ReleasesUrl -or -not (Get-Command gh -ErrorAction SilentlyContinue)) {
        return $false
    }
    & gh auth status --hostname github.com *> $null
    return $LASTEXITCODE -eq 0
}

function Receive-File([string]$Source, [string]$Destination) {
    $Uri = [Uri]$Source
    if ($Uri.Scheme -eq "file") {
        Copy-Item -LiteralPath $Uri.LocalPath -Destination $Destination
        return
    }
    if ($Uri.Scheme -ne "https") {
        Fail "the release URL $Source is not https or file. Set DISCORD_PURGE_RELEASES_URL to an https:// or file:// URL"
    }
    Invoke-WebRequest -UseBasicParsing -Uri $Uri -OutFile $Destination
}

try {
    if (-not $Repository) {
        $Repository = "racetozero/discord-purge"
    }
    $UseGitHubCli = Test-GitHubCli
    if (-not $ReleasesUrl) {
        $ReleasesUrl = "https://github.com/$Repository/releases"
    }

    if (-not $Version -or $Version -eq "latest") {
        if ($UseGitHubCli) {
            $Version = (& gh release view --repo $Repository --json tagName --jq .tagName).Trim()
            if ($LASTEXITCODE -ne 0 -or -not $Version) {
                Fail "could not find the latest release with gh"
            }
        } elseif ($env:DISCORD_PURGE_RELEASES_URL) {
            Fail "set DISCORD_PURGE_VERSION when DISCORD_PURGE_RELEASES_URL is set"
        } else {
            try {
                $Release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repository/releases/latest"
                $Version = $Release.tag_name
            } catch {
                Fail "could not find the latest release. For a private repository, run 'gh auth login' and retry"
            }
        }
    }
    $Tag, $PlainVersion = Normalize-Tag $Version

    if (-not $Target) {
        $Architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
        $Target = switch ($Architecture) {
            "X64" { "x86_64-pc-windows-msvc" }
            "Arm64" { "aarch64-pc-windows-msvc" }
            default { Fail "this Windows processor ($Architecture) is unsupported. Use an X64 or Arm64 Windows system" }
        }
    }
    if ($Target -notin @("x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc")) {
        Fail "DISCORD_PURGE_TARGET is $Target. Set it to x86_64-pc-windows-msvc or aarch64-pc-windows-msvc, or unset it to detect the processor"
    }

    if (-not $InstallDirectory) {
        if (-not $HOME) {
            Fail "HOME is not set. Set DISCORD_PURGE_INSTALL_DIR"
        }
        $InstallDirectory = Join-Path $HOME ".local\bin"
    }

    $ArchiveName = "discord-purge-$Target.zip"
    $ChecksumName = "$ArchiveName.sha256"
    $TemporaryDirectory = Join-Path ([IO.Path]::GetTempPath()) ("discord-purge-install-" + [Guid]::NewGuid())
    New-Item -ItemType Directory -Path $TemporaryDirectory | Out-Null

    try {
        Write-Host "Installing discord-purge $PlainVersion for $Target"
        if ($UseGitHubCli) {
            & gh release download $Tag --repo $Repository --pattern $ArchiveName --pattern $ChecksumName --dir $TemporaryDirectory --clobber
            if ($LASTEXITCODE -ne 0) {
                Fail "could not download $Tag with gh"
            }
        } else {
            $AssetBase = "$($ReleasesUrl.TrimEnd('/'))/download/$Tag"
            Receive-File "$AssetBase/$ArchiveName" (Join-Path $TemporaryDirectory $ArchiveName)
            Receive-File "$AssetBase/$ChecksumName" (Join-Path $TemporaryDirectory $ChecksumName)
        }

        $ArchivePath = Join-Path $TemporaryDirectory $ArchiveName
        $ChecksumPath = Join-Path $TemporaryDirectory $ChecksumName
        $Expected = ((Get-Content -LiteralPath $ChecksumPath | Where-Object { $_.Trim() } | Select-Object -First 1) -split '\s+')[0]
        if ($Expected -notmatch '^[0-9A-Fa-f]{64}$') {
            Fail "the checksum file is invalid"
        }
        $Actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $ArchivePath).Hash
        if ($Actual -ne $Expected) {
            Fail "checksum verification failed for $ArchiveName. The download is damaged or changed; nothing was installed. Run the installer again"
        }

        $ExtractDirectory = Join-Path $TemporaryDirectory "extract"
        Expand-Archive -LiteralPath $ArchivePath -DestinationPath $ExtractDirectory
        $Binary = Join-Path $ExtractDirectory "discord-purge-$Target\discord-purge.exe"
        if (-not (Test-Path -LiteralPath $Binary -PathType Leaf)) {
            $Binary = Join-Path $ExtractDirectory "discord-purge.exe"
        }
        if (-not (Test-Path -LiteralPath $Binary -PathType Leaf)) {
            Fail "the release archive does not contain discord-purge.exe"
        }

        New-Item -ItemType Directory -Force -Path $InstallDirectory | Out-Null
        $Destination = Join-Path $InstallDirectory "discord-purge.exe"
        $Staged = Join-Path $InstallDirectory (".discord-purge.install." + [Guid]::NewGuid() + ".exe")
        Copy-Item -LiteralPath $Binary -Destination $Staged
        Move-Item -Force -LiteralPath $Staged -Destination $Destination
        Write-Host "Installed discord-purge $PlainVersion to $Destination"

        $PathEntries = $env:PATH -split ';'
        if ($InstallDirectory -notin $PathEntries) {
            Write-Host "Add $InstallDirectory to PATH to run discord-purge from any directory."
        }
    } finally {
        Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $TemporaryDirectory
    }
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 1
}
