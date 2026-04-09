# check_verilog_stale.ps1 -- Staleness check for Amaranth-generated Verilog.
#
# Compares the mtime of a generated .v file against the latest mtime of any
# *.py source file under one or more source directories. Writes one of:
#
#   MISSING  -- the generated file does not exist
#   STALE    -- at least one source .py file is newer than the generated .v
#   FRESH    -- the generated .v is newer than every source .py file
#
# Intended to be called from build_fpga.bat (or equivalent) so that stale
# IP Verilog is automatically regenerated before Vivado synthesis, instead
# of silently baking yesterday's logic into today's bitstream (see
# doc/changes/009_build_staleness.md).
#
# Usage:
#   powershell -NoProfile -ExecutionPolicy Bypass -File check_verilog_stale.ps1 `
#       -VerilogFile <path\to\generated.v> `
#       -SourceDirs  "<src_dir_1>[;<src_dir_2>...]"
#
# `-SourceDirs` is a single semicolon-delimited string (not a comma array)
# because PowerShell's `-File` mode cannot bind `[string[]]` from the
# command line the way `-Command` can.
#
# Exit code is always 0; the result is the single word on stdout.

param(
    [Parameter(Mandatory = $true)]
    [string]$VerilogFile,

    [Parameter(Mandatory = $true)]
    [string]$SourceDirs
)

$dirs = $SourceDirs.Split(';') | Where-Object { $_ -ne '' }

$ErrorActionPreference = 'Stop'

if (-not (Test-Path -LiteralPath $VerilogFile)) {
    Write-Output 'MISSING'
    exit 0
}

$genTime = (Get-Item -LiteralPath $VerilogFile).LastWriteTime
$maxSrcTime = $null

foreach ($dir in $dirs) {
    if (-not (Test-Path -LiteralPath $dir)) {
        continue
    }
    $files = Get-ChildItem -LiteralPath $dir -Filter '*.py' -Recurse `
        -File -ErrorAction SilentlyContinue
    foreach ($f in $files) {
        if ($null -eq $maxSrcTime -or $f.LastWriteTime -gt $maxSrcTime) {
            $maxSrcTime = $f.LastWriteTime
        }
    }
}

if ($null -ne $maxSrcTime -and $maxSrcTime -gt $genTime) {
    Write-Output 'STALE'
} else {
    Write-Output 'FRESH'
}
