# Setup script for `--features library_wallet` builds.
# Run once before building with `cargo check --features library_wallet`.
#
# This:
#   1. Creates a .git marker in the minotari_wallet crate directory so
#      its build script (which uses `git describe`) won't panic.
#   2. Sets up the PROTOC and RUSTFLAGS environment variables needed
#      by transitive build scripts (tari_comms, tari_p2p, etc.).
#
# Usage:
#   .\setup-library-wallet.ps1
#   $env:PROTOC = ... # only needed in this session
#   cargo check --features library_wallet

Write-Host "Setting up for library_wallet builds..." -ForegroundColor Cyan

# Step 1: Create .git marker in the minotari_wallet crate directory
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { "$HOME\.cargo" }

$crateDirs = Get-ChildItem -Path "$cargoHome\registry\src" -Recurse -Depth 2 -Directory |
    Where-Object { $_.Name -like "*minotari_wallet*5.3*" }

if (-not $crateDirs) {
    Write-Warning "minotari_wallet crate not found in cargo registry at $cargoHome\registry\src."
    Write-Warning "Run 'cargo fetch' or 'cargo check --features library_wallet' (will fail) to download it first."
    exit 1
}

$crateDir = $crateDirs[0]
$gitMarker = Join-Path $crateDir ".git"
if (-not (Test-Path $gitMarker)) {
    New-Item -ItemType Directory -Path $gitMarker -Force | Out-Null
    Write-Host "  Created .git marker in $crateDir" -ForegroundColor Green
} else {
    Write-Host "  .git marker already exists in $crateDir" -ForegroundColor Green
}

# Step 2: Find vendored protoc
$protocPath = Get-ChildItem -Path "$cargoHome\registry\src" -Recurse -Depth 4 -Filter "protoc.exe" |
    Where-Object { $_.FullName -like "*protoc-bin-vendored*" } |
    Select-Object -First 1

if (-not $protocPath) {
    Write-Warning "Vendored protoc not found. Run 'cargo fetch' first."
    exit 1
}

$env:PROTOC = $protocPath.FullName
$env:RUSTFLAGS = "-C link-arg=/FORCE:MULTIPLE"

Write-Host "`nDONE."

if ($args -contains "-Build") {
    Write-Host "Running: cargo check --features library_wallet" -ForegroundColor Cyan
    cargo check --features library_wallet
    if ($LASTEXITCODE -eq 0) {
        Write-Host "Build succeeded!" -ForegroundColor Green
    } else {
        Write-Host "Build failed (exit code $LASTEXITCODE)." -ForegroundColor Red
    }
} else {
    Write-Host "To build with library_wallet in THIS terminal:"
    Write-Host "  . .\setup-library-wallet.ps1  - OR just set env vars manually:"
    Write-Host "  `$env:PROTOC    = '$env:PROTOC'"
    Write-Host "  `$env:RUSTFLAGS = '$env:RUSTFLAGS'"
    Write-Host "  cargo check --features library_wallet"
}
