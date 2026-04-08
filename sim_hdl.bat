@echo off
REM ============================================================================
REM sim_hdl.bat — Maia HDL Simulation Runner (Windows launcher)
REM
REM Runs HDL unit tests via Docker container.
REM
REM Tier 1: Amaranth Python simulator (pytest, ~1-2 min, no extra tools)
REM Tier 2: cocotb + Icarus Verilog (auto-installed, ~5 min)
REM
REM Usage:
REM   sim_hdl.bat                  Run both tiers
REM   sim_hdl.bat --tier1          Tier 1 only (fast)
REM   sim_hdl.bat --tier2          Tier 2 only (cocotb)
REM   sim_hdl.bat --test NAME      Run single test by name/substring
REM   sim_hdl.bat --vcd            Save VCD waveforms
REM   sim_hdl.bat --interactive    Open Docker shell for debugging
REM
REM Prerequisites:
REM   - Docker Desktop running
REM   - Internet connection (first run only)
REM ============================================================================
setlocal enabledelayedexpansion

echo.
echo ============================================================
echo  Maia HDL Simulation
echo ============================================================
echo.

REM ── Check Docker ────────────────────────────────────────────────────────────
docker --version >nul 2>&1
if errorlevel 1 (
    echo [ERROR] Docker is not running or not installed.
    echo Please start Docker Desktop and try again.
    pause
    exit /b 1
)
echo [OK] Docker is available.

REM ── Paths ───────────────────────────────────────────────────────────────────
set "PROJECT_DIR=%~dp0"
if "%PROJECT_DIR:~-1%"=="\" set "PROJECT_DIR=%PROJECT_DIR:~0,-1%"

set "DOCKER_IMAGE=python:3.11-slim"
set "DOCKER_VOLUME=maia-hdl-build"

REM Convert Windows path to Docker mount format (/c/Users/...)
set "WIN_PATH=%PROJECT_DIR%"
set "DRIVE=%WIN_PATH:~0,1%"
set "REST=%WIN_PATH:~2%"
set "REST=%REST:\=/%"
for %%L in (a b c d e f g h i j k l m n o p q r s t u v w x y z) do (
    if /i "%DRIVE%"=="%%L" set "DRIVE_LOWER=%%L"
)
set "DOCKER_SRC=/%DRIVE_LOWER%%REST%"

REM Verify source exists
if not exist "%PROJECT_DIR%\maia-hdl\maia_hdl\maia_sdr.py" (
    echo [ERROR] maia-hdl source not found.
    pause
    exit /b 1
)
if not exist "%PROJECT_DIR%\maia-hdl\test" (
    echo [ERROR] maia-hdl test directory not found.
    pause
    exit /b 1
)
echo [OK] Project: %PROJECT_DIR%
echo [OK] Docker mount: %DOCKER_SRC%
echo.

REM ── Parse arguments ─────────────────────────────────────────────────────────
set "EXTRA_ARGS="
set "INTERACTIVE=false"
set "MODE_DESC=Both tiers (Amaranth sim + cocotb)"

if "%~1"=="--tier1" (
    set "EXTRA_ARGS=--tier1"
    set "MODE_DESC=Tier 1 only: Amaranth Python simulator"
    goto :mode_set
)
if "%~1"=="--tier2" (
    set "EXTRA_ARGS=--tier2"
    set "MODE_DESC=Tier 2 only: cocotb + Icarus Verilog"
    goto :mode_set
)
if "%~1"=="--vcd" (
    set "EXTRA_ARGS=--vcd"
    set "MODE_DESC=Both tiers + VCD waveforms"
    goto :mode_set
)
if "%~1"=="--test" (
    set "EXTRA_ARGS=--tier1 --test %~2"
    set "MODE_DESC=Single test: %~2"
    goto :mode_set
)
if "%~1"=="--interactive" (
    set "INTERACTIVE=true"
    set "MODE_DESC=Interactive Docker shell"
    goto :mode_set
)

:mode_set
echo Mode: %MODE_DESC%
echo.

REM ── Launch Docker ───────────────────────────────────────────────────────────
if "%INTERACTIVE%"=="true" (
    echo Launching interactive Docker shell...
    echo.
    echo Inside the container, run:
    echo   bash /mnt/src/sim_hdl.sh [options]
    echo.

    docker run -it --rm ^
        -v "%PROJECT_DIR%:%DOCKER_SRC%" ^
        -v %DOCKER_VOLUME%:/root/maia_hdl_build ^
        -e "SRC_MOUNT=%DOCKER_SRC%" ^
        -w "%DOCKER_SRC%" ^
        %DOCKER_IMAGE% ^
        /bin/bash
) else (
    echo Starting simulation in Docker...
    echo   Image:  %DOCKER_IMAGE%
    echo   Volume: %DOCKER_VOLUME% (shared with build_hdl)
    echo.

    docker run -i --rm ^
        -v "%PROJECT_DIR%:%DOCKER_SRC%" ^
        -v %DOCKER_VOLUME%:/root/maia_hdl_build ^
        -e "SRC_MOUNT=%DOCKER_SRC%" ^
        -w "%DOCKER_SRC%" ^
        %DOCKER_IMAGE% ^
        /bin/bash "%DOCKER_SRC%/sim_hdl.sh" %EXTRA_ARGS%
)

if errorlevel 1 (
    echo.
    echo [ERROR] Simulation failed. Check output above.
    echo.
    echo Common fixes:
    echo   - Docker not running: Start Docker Desktop
    echo   - Import error: run build_hdl.bat first to set up venv
    echo   - Clean venv: build_hdl.bat --clean then retry
    echo.
    pause
    exit /b 1
)

echo.
echo ============================================================
echo  SIMULATION COMPLETE
echo ============================================================
echo.
