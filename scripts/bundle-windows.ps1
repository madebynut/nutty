param([ValidateSet('release', 'debug')][string]$Profile = 'release')
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$Root = Split-Path $PSScriptRoot -Parent
Push-Location $Root
try {
    if ($Profile -eq 'release') { cargo build -p nutty --release --locked }
    else { cargo build -p nutty --locked }
    if ($LASTEXITCODE -ne 0) { throw 'Cargo build failed' }
    $Metadata = cargo metadata --no-deps --format-version 1 --locked | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw 'Cargo metadata failed' }
    $Version = ($Metadata.packages | Where-Object name -eq 'nutty').version
    $Tag = if ($env:NUTTY_RELEASE_TAG) { $env:NUTTY_RELEASE_TAG } else { "v$Version" }
    if ($Tag -notmatch ('^v' + [regex]::Escape($Version) + '(-[0-9A-Za-z]+([.-][0-9A-Za-z]+)*)?$')) { throw 'Tag/version mismatch' }
    $Exe = Join-Path $Root "target/$Profile/Nutty.exe"
    $Info = (Get-Item $Exe).VersionInfo
    if ($Info.ProductName -ne 'Nutty' -or $Info.FileDescription -ne 'Nutty' -or $Info.ProductVersion -ne $Version) {
        throw "Incorrect Windows identity: $($Info | Format-List | Out-String)"
    }
    # Verify the GUI executable is callable with redirected output without opening a console.
    $Stdout = Join-Path $env:TEMP "nutty-version-$([guid]::NewGuid()).txt"
    try {
        $Process = Start-Process $Exe -ArgumentList '--version' -Wait -PassThru -RedirectStandardOutput $Stdout
        if ($Process.ExitCode -ne 0 -or (Get-Content $Stdout -Raw).Trim() -ne "Nutty $Version") { throw 'Executable version smoke test failed' }
    } finally { Remove-Item $Stdout -ErrorAction SilentlyContinue }
    Add-Type -AssemblyName System.Drawing
    $Icon = [System.Drawing.Icon]::ExtractAssociatedIcon($Exe)
    if ($null -eq $Icon) { throw 'Missing Windows application icon' }
    $Icon.Dispose()
    $Name = "Nutty-$Tag-windows-x86_64"
    $Stage = Join-Path $Root "target/bundle/$Name"
    New-Item $Stage -ItemType Directory -Force | Out-Null
    Copy-Item $Exe "$Stage/Nutty.exe"
    Copy-Item LICENSE, README.md $Stage
    Copy-Item guides/windows.md "$Stage/Windows.md"
    New-Item (Join-Path $Root 'dist') -ItemType Directory -Force | Out-Null
    Compress-Archive -Path "$Stage/*" -DestinationPath "dist/$Name.zip" -Force
    Write-Host "Created dist/$Name.zip"
} finally { Pop-Location }
