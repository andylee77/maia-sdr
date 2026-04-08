@echo off
setlocal enabledelayedexpansion

echo ============================================================
echo  Maia SDR — Clean Build Artifacts
echo ============================================================
echo.

set "PROJECT_DIR=%~dp0"
if "%PROJECT_DIR:~-1%"=="\" set "PROJECT_DIR=%PROJECT_DIR:~0,-1%"

set "MAIA_HDL=%PROJECT_DIR%\maia-hdl"
set "IP_DIR=%MAIA_HDL%\ip\maia-sdr"
set "ADI_LIB=%MAIA_HDL%\adi-hdl\library"

REM ── Parse arguments ─────────────────────────────────────────────────────────
set "CLEAN_DOCKER=false"
if "%~1"=="--docker" set "CLEAN_DOCKER=true"
if "%~1"=="--all" set "CLEAN_DOCKER=true"
if "%~1"=="--help" (
    echo Usage:
    echo   clean.bat             Clean Vivado build artifacts
    echo   clean.bat --docker    Also remove Docker build volume
    echo   clean.bat --all       Clean everything (Vivado + Docker)
    exit /b 0
)

REM ── [1/4] Clean ADI library IP cores ────────────────────────────────────────
echo [1/4] Cleaning ADI library IP cores...
set "ADI_CLEANED=0"

if exist "%ADI_LIB%" (
    for %%d in (
        "%ADI_LIB%\axi_ad9361\axi_ad9361.gen"
        "%ADI_LIB%\axi_ad9361\axi_ad9361.srcs"
        "%ADI_LIB%\axi_ad9361\axi_ad9361.cache"
        "%ADI_LIB%\axi_ad9361\axi_ad9361.ip_user_files"
        "%ADI_LIB%\axi_dmac\axi_dmac.gen"
        "%ADI_LIB%\axi_dmac\axi_dmac.srcs"
        "%ADI_LIB%\axi_dmac\axi_dmac.cache"
        "%ADI_LIB%\axi_dmac\axi_dmac.ip_user_files"
        "%ADI_LIB%\util_axis_fifo\util_axis_fifo.gen"
        "%ADI_LIB%\util_axis_fifo\util_axis_fifo.srcs"
        "%ADI_LIB%\util_axis_fifo\util_axis_fifo.cache"
        "%ADI_LIB%\util_axis_fifo\util_axis_fifo.ip_user_files"
        "%ADI_LIB%\util_cdc\util_cdc.gen"
        "%ADI_LIB%\util_cdc\util_cdc.srcs"
        "%ADI_LIB%\util_cdc\util_cdc.cache"
        "%ADI_LIB%\util_cdc\util_cdc.ip_user_files"
        "%ADI_LIB%\util_pack\util_cpack2\util_cpack2.gen"
        "%ADI_LIB%\util_pack\util_cpack2\util_cpack2.srcs"
        "%ADI_LIB%\util_pack\util_cpack2\util_cpack2.cache"
        "%ADI_LIB%\util_pack\util_cpack2\util_cpack2.ip_user_files"
        "%ADI_LIB%\util_pack\util_upack2\util_upack2.gen"
        "%ADI_LIB%\util_pack\util_upack2\util_upack2.srcs"
        "%ADI_LIB%\util_pack\util_upack2\util_upack2.cache"
        "%ADI_LIB%\util_pack\util_upack2\util_upack2.ip_user_files"
    ) do (
        if exist "%%~d" (
            rmdir /s /q "%%~d" 2>nul
            echo   Removed: %%~d
            set /a ADI_CLEANED+=1
        )
    )
    :: Also remove .xpr files
    for %%f in (
        "%ADI_LIB%\axi_ad9361\axi_ad9361.xpr"
        "%ADI_LIB%\axi_dmac\axi_dmac.xpr"
        "%ADI_LIB%\util_axis_fifo\util_axis_fifo.xpr"
        "%ADI_LIB%\util_cdc\util_cdc.xpr"
        "%ADI_LIB%\util_pack\util_cpack2\util_cpack2.xpr"
        "%ADI_LIB%\util_pack\util_upack2\util_upack2.xpr"
    ) do (
        if exist "%%~f" (
            del /q "%%~f" 2>nul
            set /a ADI_CLEANED+=1
        )
    )
)
if "!ADI_CLEANED!"=="0" (
    echo   Nothing to clean.
) else (
    echo [OK] ADI library cleaned (!ADI_CLEANED! items).
)
echo.

REM ── [2/4] Clean Maia SDR IP cores ──────────────────────────────────────────
echo [2/4] Cleaning Maia SDR IP cores...
set "IP_CLEANED=0"

for %%c in (default maia_iio maia_iio_lite) do (
    if exist "%IP_DIR%\%%c" (
        rmdir /s /q "%IP_DIR%\%%c" 2>nul
        echo   Removed: ip\maia-sdr\%%c\
        set /a IP_CLEANED+=1
    )
)
:: Clean generated SVD
if exist "%MAIA_HDL%\maia-sdr.svd" (
    del /q "%MAIA_HDL%\maia-sdr.svd" 2>nul
    echo   Removed: maia-hdl\maia-sdr.svd
    set /a IP_CLEANED+=1
)
if "!IP_CLEANED!"=="0" (
    echo   Nothing to clean.
) else (
    echo [OK] IP cores cleaned (!IP_CLEANED! items).
)
echo.

REM ── [3/4] Clean FPGA project artifacts ─────────────────────────────────────
echo [3/4] Cleaning FPGA project artifacts...
set "FPGA_CLEANED=0"

for %%p in (fishball7020_iio fishball_iio pluto_iio plutoplus_iio e200_iio libre_iio) do (
    set "PROJ=%MAIA_HDL%\projects\%%p"
    REM Project name is the first part before _iio, or 'fishball' for fishball variants
    set "PNAME=fishball"
    if "%%p"=="pluto_iio" set "PNAME=pluto"
    if "%%p"=="plutoplus_iio" set "PNAME=plutoplus"
    if "%%p"=="e200_iio" set "PNAME=e200"
    if "%%p"=="libre_iio" set "PNAME=libre"

    for %%d in (
        "!PROJ!\!PNAME!.cache"
        "!PROJ!\!PNAME!.gen"
        "!PROJ!\!PNAME!.hw"
        "!PROJ!\!PNAME!.ip_user_files"
        "!PROJ!\!PNAME!.runs"
        "!PROJ!\!PNAME!.sdk"
        "!PROJ!\!PNAME!.srcs"
        "!PROJ!\.Xil"
    ) do (
        if exist "%%~d" (
            rmdir /s /q "%%~d" 2>nul
            echo   Removed: %%~d
            set /a FPGA_CLEANED+=1
        )
    )
    if exist "!PROJ!\!PNAME!.xpr" (
        del /q "!PROJ!\!PNAME!.xpr" 2>nul
        set /a FPGA_CLEANED+=1
    )
)

:: Clean simulation output
if exist "%PROJECT_DIR%\sim_output" (
    rmdir /s /q "%PROJECT_DIR%\sim_output" 2>nul
    echo   Removed: sim_output\
    set /a FPGA_CLEANED+=1
)

if "!FPGA_CLEANED!"=="0" (
    echo   Nothing to clean.
) else (
    echo [OK] FPGA projects cleaned (!FPGA_CLEANED! items).
)
echo.

REM ── [4/4] Clean Docker build volume (optional) ─────────────────────────────
if "%CLEAN_DOCKER%"=="true" (
    echo [4/4] Cleaning Docker build volume...
    docker volume rm maia-hdl-build 2>nul
    if !errorlevel! equ 0 (
        echo [OK] Docker volume 'maia-hdl-build' removed.
        echo     Next build_hdl.bat run will recreate venv from scratch.
    ) else (
        echo   Volume 'maia-hdl-build' not found or Docker not running.
    )
) else (
    echo [4/4] Docker volume: SKIPPED (use --docker or --all to clean)
)
echo.

REM ── Summary ─────────────────────────────────────────────────────────────────
echo ============================================================
echo  Clean complete!
echo ============================================================
echo.
echo  To rebuild:
echo    build_hdl.bat     — Regenerate Verilog + SVD
echo    build_fpga.bat    — Full Vivado FPGA synthesis
echo    sim_hdl.bat       — Run HDL simulations
echo.
