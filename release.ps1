<#
.SYNOPSIS
Builds Godspeed and LordSpeed and publishes them as a GitHub release. Both apps
check for it at launch and once a day, and offer it as a one-click update.

.EXAMPLE
./release.ps1 -Version 0.2.1 -Notes "Faster search in big folders"
#>
param(
    [Parameter(Mandatory)][string]$Version,
    [string]$Notes = "",
    [string]$Repo = "TheRedViperV/godspeed"
)
# Not "Stop": Windows PowerShell treats cargo's progress output (on stderr) as an
# error then. Every native command's exit code is checked below instead.
Set-Location $PSScriptRoot
if ($Version -notmatch '^\d+\.\d+\.\d+$') { throw "Version must look like 1.2.3" }

# 1. The version both apps report, and compare against what's published.
$rx = [regex]'(?m)^version = "[^"]*"'
foreach ($f in "Cargo.toml", "lordspeed\Cargo.toml") {
    $path = Join-Path $PSScriptRoot $f
    $text = $rx.Replace([IO.File]::ReadAllText($path), "version = `"$Version`"", 1)
    [IO.File]::WriteAllText($path, $text) # no byte-order mark: Cargo won't read one
}

# 2. Build both apps.
cargo build --release --workspace
if ($LASTEXITCODE) { throw "The build failed." }

# 3. The files to publish, and latest.txt: the version, plus each app's download
#    link and SHA-256, which the apps check before using a download.
$out = Join-Path $PSScriptRoot "dist\v$Version"
New-Item -ItemType Directory -Force $out | Out-Null
$base = "https://github.com/$Repo/releases/download/v$Version"
$lines = @("version=$Version")
foreach ($app in "godspeed", "lordspeed") {
    Copy-Item "target\release\$app.exe" "$out\$app.exe" -Force
    $hash = (Get-FileHash "$out\$app.exe" -Algorithm SHA256).Hash.ToLower()
    $lines += "$app=$base/$app.exe"
    $lines += "${app}_sha256=$hash"
}
[IO.File]::WriteAllText("$out\latest.txt", ($lines -join "`n") + "`n")

# 4. The public repo (code and releases live together),
#    created on first use.
gh repo view $Repo *> $null
if ($LASTEXITCODE) {
    gh repo create $Repo --public --add-readme --description "Godspeed: instant file search and disk space analyzer for Windows"
    if ($LASTEXITCODE) { throw "Couldn't create $Repo." }
}

# 5. Publish. It becomes the "latest" release, which is where the apps look.
if (-not $Notes) { $Notes = "Godspeed and LordSpeed $Version" }
gh release create "v$Version" "$out\godspeed.exe" "$out\lordspeed.exe" "$out\latest.txt" --repo $Repo --title "v$Version" --notes $Notes
# gh can report failure even when everything arrived, so check what's actually there.
$names = (gh release view "v$Version" --repo $Repo --json assets | ConvertFrom-Json).assets | Where-Object state -eq "uploaded" | ForEach-Object name
foreach ($f in "godspeed.exe", "lordspeed.exe", "latest.txt") {
    if ($names -notcontains $f) { throw "Publishing failed: $f isn't on the release." }
}
"Published v${Version}: https://github.com/$Repo/releases/tag/v$Version"
