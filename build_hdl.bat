@echo off
REM ============================================================================
REM build_hdl.bat — Maia HDL Verilog + SVD Generator (Windows launcher)
REM
REM Generates maia_sdr.v and maia-sdr.svd from Amaranth HDL Python source
REM using a Docker container with a persistent build volume.
REM
REM Usage:
REM   build_hdl.bat                  Full build (Verilog + SVD)
REM   build_hdl.bat --verilog-only   Verilog only (skip SVD)
REM   build_hdl.bat --svd-only       SVD only (skip Verilog)
REM   build_hdl.bat --clean          Clean cached venv, rebuild from scratch
REM   build_hdl.bat --interactive    Open Docker shell for debugging
REM   build_hdl.bat --config NAME    Use specific Amaranth config (default: maia_iio)
REM
REM Prerequisites:
REM   - Docker Desktop running
REM   - Internet connection (first run only, for pip install)
REM   - ~500 MB free disk space
REM
REM Output:
REM   maia-hdl/ip/maia-sdr/<config>/maia_sdr.v   Verilog netlist
REM   maia-hdl/maia-sdr.svd                       SVD register map
REM ============================================================================
setlocal enabledelayedexpansion

echo.
echo ============================================================
echo  Maia HDL Verilog + SVD Generator
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

REM Verify maia-hdl source exists
if not exist "%PROJECT_DIR%\maia-hdl\maia_hdl\maia_sdr.py" (
    echo [ERROR] maia-hdl source not found at:
    echo   %PROJECT_DIR%\maia-hdl\maia_hdl\maia_sdr.py
    pause
    exit /b 1
)
echo [OK] Project: %PROJECT_DIR%
echo [OK] Docker mount: %DOCKER_SRC%
echo.

REM ── Parse arguments ─────────────────────────────────────────────────────────
set "EXTRA_ARGS="
set "INTERACTIVE=false"
set "MODE_DESC=Full build (Verilog + SVD)"

if "%~1"=="--verilog-only" (
    set "EXTRA_ARGS=--verilog-only"
    set "MODE_DESC=Verilog only"
    goto :mode_set
)
if "%~1"=="--svd-only" (
    set "EXTRA_ARGS=--svd-only"
    set "MODE_DESC=SVD only"
    goto :mode_set
)
if "%~1"=="--clean" (
    set "EXTRA_ARGS=--clean"
    set "MODE_DESC=Clean + full rebuild"
    goto :mode_set
)
if "%~1"=="--interactive" (
    set "INTERACTIVE=true"
    set "MODE_DESC=Interactive Docker shell"
    goto :mode_set
)
if "%~1"=="--config" (
    set "EXTRA_ARGS=--config %~2"
    set "MODE_DESC=Config: %~2"
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
    echo   bash /mnt/src/build_hdl.sh
    echo.

    docker run -it --rm ^
        -v "%PROJECT_DIR%:%DOCKER_SRC%" ^
        -v %DOCKER_VOLUME%:/root/maia_hdl_build ^
        -e "SRC_MOUNT=%DOCKER_SRC%" ^
        -w "%DOCKER_SRC%" ^
        %DOCKER_IMAGE% ^
        /bin/bash
) else (
    echo Starting HDL build in Docker...
    echo   Image:  %DOCKER_IMAGE%
    echo   Volume: %DOCKER_VOLUME% (persistent venv cache)
    echo   Source: %DOCKER_SRC%
    echo.

    docker run -i --rm ^
        -v "%PROJECT_DIR%:%DOCKER_SRC%" ^
        -v %DOCKER_VOLUME%:/root/maia_hdl_build ^
        -e "SRC_MOUNT=%DOCKER_SRC%" ^
        -w "%DOCKER_SRC%" ^
        %DOCKER_IMAGE% ^
        /bin/bash "%DOCKER_SRC%/build_hdl.sh" %EXTRA_ARGS%
)

if errorlevel 1 (
    echo.
    echo [ERROR] HDL build failed. Check output above.
    echo.
    echo Common fixes:
    echo   - Docker not running: Start Docker Desktop
    echo   - No internet: needed first time for pip install
    echo   - Clean rebuild: build_hdl.bat --clean
    echo.
    pause
    exit /b 1
)

echo.
echo ============================================================
echo  HDL BUILD COMPLETE
echo ============================================================
echo.
echo Output files:
if not "%~1"=="--svd-only" (
    echo   maia-hdl\ip\maia-sdr\maia_iio\maia_sdr.v   (Verilog netlist)
)
if not "%~1"=="--verilog-only" (
    echo   maia-hdl\maia-sdr.svd                       (SVD register map)
)
echo.
echo Next: build_fpga.bat to synthesize FPGA bitstream
echo.
