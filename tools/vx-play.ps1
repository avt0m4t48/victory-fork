# vx-layer launcher: runs your own dump of an Xbox Series title with this build, with the speed
# settings measured on an RTX 4090 (see README, "vx-layer"). Offline by design; the game folder is only
# read. Nothing here is specific to one machine: every path is a parameter.
#
#   powershell -ExecutionPolicy Bypass -File tools\vx-play.ps1 -GameExe "<game folder>\<title exe>"
#   ... -Config <victory.toml>   config file (default: victory.toml next to the repository root)
#   ... -Exe <victory.exe>       build to run (default: target\release\victory.exe)
#   ... -SeriesS                 the Series S profile (its FSR4 upscaler does not run on NVIDIA yet)
#   ... -NoSpeedSwitches         the fork's defaults instead of the settings below
#   ... -NoWide                  without VICTORY_WAVE_WIDE_ONLY (faster, but unlit characters and rooms)
#   ... -NoOverlay               without the status bar (tools\vx-overlay.py, needs Python 3 with Tk) that sits on
#                                the game window's bottom edge and says WORKING (not frozen) when frames stall
#   ... -Logs <folder>           where stdout/stderr go (default: .\runs)
#
# Keyboard is a pad while the window has focus: Enter = A, Backspace = B, W A S D = left stick,
# I J K L = right stick, arrows = D-pad, Q / E shoulders, Z / C triggers, Esc = Menu, Tab = View.
# The window title shows fps, ms per frame and draws per frame.
param(
    [Parameter(Mandatory = $true)][string]$GameExe,
    [string]$Exe = (Join-Path $PSScriptRoot '..\target\release\victory.exe'),
    [string]$Config = (Join-Path $PSScriptRoot '..\victory.toml'),
    [string]$Logs = (Join-Path (Get-Location) 'runs'),
    [switch]$SeriesS,
    [switch]$NoSpeedSwitches,
    [switch]$NoWide,
    [switch]$NoOverlay
)
if (-not (Test-Path $Exe)) { throw "no build at ${Exe}: cargo build --release -p victory" }
if (-not (Test-Path $GameExe)) { throw "no title executable at ${GameExe}" }
# Test and debug switches never carry into normal play (the idea of ztrx's launcher).
foreach ($v in 'VICTORY_KEYS', 'VICTORY_CAPTURE', 'VICTORY_CAPTURE_FRAMES', 'VICTORY_DUMP_TARGETS', 'VICTORY_LOG') { Remove-Item "Env:$v" -ErrorAction SilentlyContinue }
if (Test-Path $Config) { $env:VICTORY_CONFIG = (Resolve-Path $Config).Path }
# textures the title rewrites through another mapping are hashed every frame while they change (no
# startup flicker; ~5-7 ms/frame)
$env:VICTORY_TEXTURE_HOT = '1'
# the visibility and tiled light-list passes run as 32-lane waves on 32-lane GPUs (NVIDIA); without this
# they are refused and dropped, and characters and rooms render unlit. Drop with -NoWide for speed.
if (-not $NoWide) { $env:VICTORY_WAVE_WIDE_ONLY = '369cbe906ea72764,2cc4c197ee29e291' }
if (-not $NoSpeedSwitches) {
    # texture refreshes read title memory through the kernel instead of waiting for the title's remaps
    $env:VICTORY_TEXTURE_NO_WAIT = '1'
    # a frame waits at most 24 ms for pipelines still being built (draws without one are left out)
    $env:VICTORY_PIPELINE_WAIT_MS = '24'
    # small pipelines first; big compute programs on fewer workers (vx-layer)
    $env:VICTORY_PIPELINE_SMALL_FIRST = '1'
    # the fork's own submission and card-buffer speed-ups
    $env:VICTORY_LATE_WAIT = '3'
    $env:VICTORY_WIDE_BACK = '1'
    $env:VICTORY_CARD_BUFFERS_MIN_MB = '1'
    $env:VICTORY_CARD_BUFFERS_MB = '2048'
}
New-Item -ItemType Directory -Force $Logs | Out-Null
$tag = 'play-' + (Get-Date -Format 'yyyyMMdd-HHmmss')
$console = if ($SeriesS) { 'series-s' } else { 'series-x' }
$p = Start-Process -FilePath $Exe -ArgumentList 'run', "`"$GameExe`"", '--execute', '--console', $console -RedirectStandardOutput (Join-Path $Logs "$tag-stdout.txt") -RedirectStandardError (Join-Path $Logs "$tag-stderr.txt") -PassThru
"started ($console), pid $($p.Id); log: $(Join-Path $Logs "$tag-stderr.txt")"
$overlay = Join-Path $PSScriptRoot 'vx-overlay.py'
$pythonw = (Get-Command pythonw -ErrorAction SilentlyContinue).Source
if (-not $NoOverlay -and $pythonw -and (Test-Path $overlay)) {
    Start-Process -FilePath $pythonw -ArgumentList "`"$overlay`"", '--pid', $p.Id, '--log', "`"$(Join-Path $Logs "$tag-stderr.txt")`"" | Out-Null
    'status bar: on (-NoOverlay to turn it off)'
}
