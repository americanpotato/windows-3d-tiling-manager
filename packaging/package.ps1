# Builds the release exe and writes dist\Window3D-<version>.zip (window3d.exe + README.txt).
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot
Push-Location $root
try {
    # Keep the user's name out of the exe: build paths (crate sources under the home folder)
    # become "~", and the PDB path keeps only the file name. A target dir of its own, so the
    # changed flags don't rebuild the everyday target\release.
    $env:CARGO_ENCODED_RUSTFLAGS = @(
        "--remap-path-prefix=$env:USERPROFILE=~",
        "-Clink-arg=/PDBALTPATH:%_PDB%"
    ) -join [char]0x1f
    cargo build --release --target-dir target\dist
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
    Remove-Item Env:CARGO_ENCODED_RUSTFLAGS
    $exe = 'target\dist\release\window3d.exe'
    $name = Split-Path -Leaf $env:USERPROFILE
    if (Select-String -Path $exe -Pattern $name -SimpleMatch -Quiet) {
        throw "the exe still contains '$name'"
    }
    $version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
    $stage = Join-Path $root "dist\Window3D-$version"
    $zip = "$stage.zip"
    Remove-Item -Recurse -Force $stage, $zip -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force $stage | Out-Null
    Copy-Item $exe, packaging\README.txt $stage
    Compress-Archive -Path "$stage\*" -DestinationPath $zip
    Remove-Item -Recurse -Force $stage
    Get-Item $zip | Select-Object FullName, Length
}
finally {
    Pop-Location
}
