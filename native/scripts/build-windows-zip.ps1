# Package the native client for Windows as a zip.
#
#   pwsh native/scripts/build-windows-zip.ps1 [-Binary PATH] [-OutDir DIR]
#
# Binary defaults to native/target/release/synara.exe. Build it on Windows
# with the MSVC toolchain: GPUI's release build compiles its shaders with
# fxc.exe from the Windows SDK, which cross builds from Linux do not have.
#
# The zip holds the client only. It connects to a server with --url/--token,
# supervises a source checkout with --spawn (needs bun.exe on PATH), or uses
# --instabox. Release builds are GUI apps with no console window.
param(
  [string]$Binary,
  [string]$OutDir
)
$ErrorActionPreference = "Stop"

$root = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
if (-not $Binary) { $Binary = Join-Path $root "native/target/release/synara.exe" }
if (-not $OutDir) { $OutDir = Join-Path $root "native/dist" }
if (-not (Test-Path $Binary)) { throw "no binary at $Binary" }

# A GUI-subsystem binary detaches from the console, so read --version through
# a pipe, which waits for it to exit.
$version = (& $Binary --version | Out-String).Trim().Split(" ")[1]
if (-not $version) { throw "could not read the version from $Binary" }

$arch = switch ($env:PROCESSOR_ARCHITECTURE) {
  "ARM64" { "arm64" }
  default { "x64" }
}
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("synara-native-" + [guid]::NewGuid())
$app = Join-Path $work "Synara Native"
New-Item -ItemType Directory -Force -Path $app | Out-Null
try {
  Copy-Item $Binary (Join-Path $app "synara.exe")
  Copy-Item (Join-Path $root "apps/desktop/resources/icon.ico") (Join-Path $app "synara.ico")
  @"
Synara Native $version

Run synara.exe with one of:
  synara.exe --url http://127.0.0.1:3773 --token TOKEN   existing server
  synara.exe --spawn C:\path\to\synara                    supervise apps/server (needs bun)
  synara.exe --instabox NAME                              server in an instabox sandbox
"@ | Set-Content -Encoding utf8 (Join-Path $app "README.txt")

  New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
  $target = Join-Path $OutDir "Synara-Native-$version-windows-$arch.zip"
  if (Test-Path $target) { Remove-Item $target }
  Compress-Archive -Path $app -DestinationPath $target
  Write-Output $target
} finally {
  Remove-Item -Recurse -Force $work
}
