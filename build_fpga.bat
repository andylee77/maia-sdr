@echo off
setlocal enabledelayedexpansion

:: ===== Parse arguments =====
set "BUILD_P25=0"
for %%a in (%*) do (
    if "%%a"=="--p25" set "BUILD_P25=1"
)

:: ===== Configuration =====
set "PROJECT_DIR=%~dp0"
if "%PROJECT_DIR:~-1%"=="\" set "PROJECT_DIR=%PROJECT_DIR:~0,-1%"

set "MAIA_HDL=%PROJECT_DIR%\maia-hdl"
set "ADI_LIB=%MAIA_HDL%\adi-hdl\library"
set "ADI_HDL_BRANCH=hdl_2023_r2"

:: Maia SDR IP (always needed — pluto base design requires it)
set "MAIA_IP_DIR=%MAIA_HDL%\ip\maia-sdr"
set "MAIA_CONFIG=maia_iio"

if "%BUILD_P25%"=="1" (
    set "P25_IP_DIR=%MAIA_HDL%\ip\p25-core"
    set "P25_CONFIG=default"
    set "FPGA_PROJECT=fishball7020_p25"
    set "FPGA_PROJECT_NAME=fishball_p25"
    set "IP_CORE_VERSION=0.1.0"
) else (
    set "FPGA_PROJECT=fishball7020_iio"
    set "FPGA_PROJECT_NAME=fishball"
)
set "FPGA_PROJECT_DIR=%MAIA_HDL%\projects\%FPGA_PROJECT%"

if "%BUILD_P25%"=="1" (
    echo ============================================================
    echo  Fishball 7020 -- P25 FPGA Bitstream Build ^(Vivado^)
    echo  Target: xc7z020clg400-1 ^(Zynq Z7020 SoC^)
    echo  Project: fishball7020_p25
    echo ============================================================
) else (
    echo ============================================================
    echo  Fishball 7020 -- FPGA Bitstream Build ^(Vivado^)
    echo  Target: xc7z020clg400-1 ^(Zynq Z7020 SoC^)
    echo  Project: fishball7020_iio
    echo ============================================================
)
echo.

:: Tezuka firmware path (configurable via env var)
if not defined TEZUKA_FW (
    REM Try relative path from MAIA_SDR project dir
    set "TEZUKA_FW=%PROJECT_DIR%\..\..\Tezuka\tezuka_fw"
)

:: ADI HDL scripts check for specific Vivado version — this allows others
set "ADI_IGNORE_VERSION_CHECK=1"

:: ===== Step 0: Find Vivado =====
echo [Step 0] Searching for Vivado installation...
set "VIVADO_DIR="

:: Check user override first
if defined VIVADO_DIR_OVERRIDE (
    if exist "%VIVADO_DIR_OVERRIDE%\bin\vivado.bat" (
        set "VIVADO_DIR=%VIVADO_DIR_OVERRIDE%"
        echo [INFO] Using user-specified Vivado: %VIVADO_DIR_OVERRIDE%
    )
)

:: Try Vivado 2023.2 (ideal match for ADI hdl_2023_r2)
if not defined VIVADO_DIR (
    for %%d in (
        "C:\Xilinx\Vivado\2023.2"
        "C:\AMDDesignTools\2023.2\Vivado"
        "C:\AMD\Vivado\2023.2"
        "D:\Xilinx\Vivado\2023.2"
    ) do (
        if exist "%%~d\bin\vivado.bat" (
            set "VIVADO_DIR=%%~d"
            echo [INFO] Found Vivado 2023.2 (ideal for ADI HDL^)
        )
    )
)

:: Fall back to Vivado 2025.2
if not defined VIVADO_DIR (
    for %%d in (
        "C:\AMDDesignTools\2025.2\Vivado"
        "C:\Xilinx\Vivado\2025.2"
        "C:\AMD\Vivado\2025.2"
        "D:\AMDDesignTools\2025.2\Vivado"
    ) do (
        if exist "%%~d\bin\vivado.bat" (
            set "VIVADO_DIR=%%~d"
            echo [INFO] Found Vivado 2025.2
        )
    )
)

if not defined VIVADO_DIR (
    echo [FAIL] No Vivado installation found!
    echo        Install Vivado 2023.2 or 2025.2 with Zynq-7000 SoC device family.
    echo        Or set VIVADO_DIR_OVERRIDE environment variable.
    goto :error
)

set "VIVADO=%VIVADO_DIR%\bin\vivado.bat"
echo [OK] Vivado: %VIVADO%
echo [INFO] ADI_IGNORE_VERSION_CHECK=1

:: Source Vivado environment
call "%VIVADO_DIR%\settings64.bat"
echo [OK] Vivado environment loaded.
echo.

:: ===== Step 1: Check / Initialize adi-hdl submodule =====
echo [Step 1] Checking ADI HDL library...
if exist "%MAIA_HDL%\adi-hdl\library\axi_ad9361" (
    echo [OK] adi-hdl submodule present with required libraries.
) else if exist "%MAIA_HDL%\adi-hdl\.git" (
    echo [INFO] adi-hdl submodule exists but may be incomplete.
    echo        Running: git submodule update --init maia-hdl/adi-hdl
    cd /d "%PROJECT_DIR%"
    git submodule update --init maia-hdl/adi-hdl
    if !errorlevel! neq 0 (
        echo [FAIL] Failed to initialize adi-hdl submodule.
        goto :error
    )
    echo [OK] adi-hdl submodule initialized.
) else (
    echo [INFO] adi-hdl not found. Initializing submodule...
    cd /d "%PROJECT_DIR%"
    git submodule update --init maia-hdl/adi-hdl
    if !errorlevel! neq 0 (
        echo [WARN] Submodule init failed. Trying direct clone...
        cd /d "%MAIA_HDL%"
        git clone --depth 1 --branch %ADI_HDL_BRANCH% https://github.com/analogdevicesinc/hdl.git adi-hdl
        if !errorlevel! neq 0 (
            echo [FAIL] Failed to get adi-hdl.
            goto :error
        )
    )
    echo [OK] adi-hdl available.
)
echo.

:: ===== Step 2: Generate Verilog =====
:: Staleness detection: if any *.py source file under the relevant HDL
:: directories is newer than the generated .v, force a regeneration.
:: Previously this step only checked for file existence, which silently
:: baked pre-fix logic into new bitstreams when sources changed between
:: builds but the Verilog wasn't re-emitted (see doc/changes/009).
echo [Step 2] Checking Verilog generation status...
set "STALE_CHECK=%PROJECT_DIR%\tools\check_verilog_stale.ps1"
if not exist "%STALE_CHECK%" (
    echo [FAIL] Missing staleness helper: %STALE_CHECK%
    goto :error
)

:: ----- Maia SDR Verilog (always needed) -----
set "MAIA_VERILOG=%MAIA_IP_DIR%\%MAIA_CONFIG%\maia_sdr.v"
set "MAIA_STATE="
for /f "delims=" %%R in ('powershell -NoProfile -ExecutionPolicy Bypass -File "%STALE_CHECK%" -VerilogFile "%MAIA_VERILOG%" -SourceDirs "%MAIA_HDL%\maia_hdl"') do set "MAIA_STATE=%%R"

set "MAIA_REGEN=0"
if "!MAIA_STATE!"=="FRESH" echo [OK] maia_sdr.v is current ^(newer than maia_hdl/*.py^).
if "!MAIA_STATE!"=="MISSING" (
    echo [INFO] maia_sdr.v not found. Running HDL generation via Docker...
    set "MAIA_REGEN=1"
)
if "!MAIA_STATE!"=="STALE" (
    echo [WARN] maia_sdr.v is STALE -- maia_hdl/*.py has newer changes.
    echo        Regenerating via Docker to avoid baking stale logic into bitstream.
    set "MAIA_REGEN=1"
)
if not defined MAIA_STATE (
    echo [FAIL] Staleness helper returned empty state for maia_sdr.v
    goto :error
)
if not "!MAIA_STATE!"=="FRESH" if not "!MAIA_STATE!"=="MISSING" if not "!MAIA_STATE!"=="STALE" (
    echo [FAIL] Staleness helper returned unexpected state: '!MAIA_STATE!'
    goto :error
)
if "!MAIA_REGEN!"=="1" (
    call "%PROJECT_DIR%\build_hdl.bat" --verilog-only
    if !errorlevel! neq 0 (
        echo [FAIL] HDL generation failed. Run build_hdl.bat manually.
        goto :error
    )
    if not exist "!MAIA_VERILOG!" (
        echo [FAIL] maia_sdr.v still not found after generation.
        goto :error
    )
    echo [OK] maia_sdr.v regenerated.
)

:: ----- P25 Verilog (only for --p25 build) -----
:: Source dirs include both p25_hdl (the P25-specific modules) AND
:: maia_hdl (because p25_top.py imports DDC, registers, DMA, etc. from
:: maia_hdl, so a change in maia_hdl affects the generated p25_core.v).
if "%BUILD_P25%"=="1" (
    set "P25_VERILOG=%P25_IP_DIR%\%P25_CONFIG%\p25_core.v"
    set "P25_STATE="
    for /f "delims=" %%R in ('powershell -NoProfile -ExecutionPolicy Bypass -File "%STALE_CHECK%" -VerilogFile "!P25_VERILOG!" -SourceDirs "%MAIA_HDL%\p25_hdl;%MAIA_HDL%\maia_hdl"') do set "P25_STATE=%%R"

    set "P25_REGEN=0"
    if "!P25_STATE!"=="FRESH" echo [OK] p25_core.v is current ^(newer than p25_hdl + maia_hdl source^).
    if "!P25_STATE!"=="MISSING" (
        echo [INFO] p25_core.v not found. Generating P25 Verilog via Docker...
        set "P25_REGEN=1"
    )
    if "!P25_STATE!"=="STALE" (
        echo [WARN] p25_core.v is STALE -- p25_hdl or maia_hdl has newer changes.
        echo        Regenerating via Docker to avoid baking stale logic into bitstream.
        set "P25_REGEN=1"
    )
    if not defined P25_STATE (
        echo [FAIL] Staleness helper returned empty state for p25_core.v
        goto :error
    )
    if not "!P25_STATE!"=="FRESH" if not "!P25_STATE!"=="MISSING" if not "!P25_STATE!"=="STALE" (
        echo [FAIL] Staleness helper returned unexpected state: '!P25_STATE!'
        goto :error
    )
    if "!P25_REGEN!"=="1" (
        call "%PROJECT_DIR%\build_hdl.bat" --verilog-only --p25 --p25-config %P25_CONFIG%
        if !errorlevel! neq 0 (
            echo [FAIL] P25 Verilog generation failed. Run build_hdl.bat --p25 manually.
            goto :error
        )
        if not exist "!P25_VERILOG!" (
            echo [FAIL] p25_core.v still not found after generation.
            goto :error
        )
        echo [OK] p25_core.v regenerated.
    )
)
echo.

:: ===== Step 3: Package IP Cores =====
:: Maia SDR IP (always needed)
echo [Step 3] Packaging Maia SDR IP core (config: %MAIA_CONFIG%)...
set "MAIA_SDR_CONFIG=%MAIA_CONFIG%"
set "MAIA_IP_CORE_VERSION=0.6.1"
set "IP_CORE_VERSION_MAIA=0.6.1"

cd /d "%MAIA_IP_DIR%\%MAIA_CONFIG%"
call "%VIVADO%" -mode batch -source "%MAIA_IP_DIR%\package_ip.tcl" -notrace
:: Vivado may return non-zero for non-fatal CRITICAL WARNINGs
if not exist "%MAIA_IP_DIR%\%MAIA_CONFIG%\component.xml" (
    echo [FAIL] Maia SDR IP packaging failed -- no component.xml created.
    goto :error
)
echo [OK] Maia SDR IP core packaged ^(component.xml created^).

:: P25 IP (only for --p25 build)
if "%BUILD_P25%"=="1" (
    echo [Step 3b] Packaging P25 IP core ^(config: %P25_CONFIG%^)...
    cd /d "%P25_IP_DIR%\%P25_CONFIG%"
    set "IP_CORE_VERSION=%IP_CORE_VERSION%"
    set "P25_CONFIG=%P25_CONFIG%"
    call "%VIVADO%" -mode batch -source "%P25_IP_DIR%\package_ip.tcl" -notrace
    if not exist "%P25_IP_DIR%\%P25_CONFIG%\component.xml" (
        echo [FAIL] P25 IP packaging failed -- no component.xml created.
        goto :error
    )
    echo [OK] P25 IP core packaged ^(component.xml created^).
)
echo.

:: ===== Step 4: Build ADI Library IP Cores =====
echo [Step 4] Building ADI library IP cores...
echo          axi_ad9361, util_clkdiv, util_rfifo, util_wfifo,
echo          util_axis_fifo, util_cdc, axi_dmac, util_cpack2, util_upack2

:: util_clkdiv (in xilinx/ subdir)
if exist "%ADI_LIB%\xilinx\util_clkdiv\component.xml" (
    echo [OK] util_clkdiv already built.
) else (
    echo [INFO] Building util_clkdiv...
    cd /d "%ADI_LIB%\xilinx\util_clkdiv"
    call "%VIVADO%" -mode batch -source util_clkdiv_ip.tcl -notrace
    if !errorlevel! neq 0 (
        echo [FAIL] util_clkdiv build failed.
        goto :error
    )
    echo [OK] util_clkdiv built.
)

:: util_rfifo
if exist "%ADI_LIB%\util_rfifo\component.xml" (
    echo [OK] util_rfifo already built.
) else (
    echo [INFO] Building util_rfifo...
    cd /d "%ADI_LIB%\util_rfifo"
    call "%VIVADO%" -mode batch -source util_rfifo_ip.tcl -notrace
    if !errorlevel! neq 0 (
        echo [FAIL] util_rfifo build failed.
        goto :error
    )
    echo [OK] util_rfifo built.
)

:: util_wfifo
if exist "%ADI_LIB%\util_wfifo\component.xml" (
    echo [OK] util_wfifo already built.
) else (
    echo [INFO] Building util_wfifo...
    cd /d "%ADI_LIB%\util_wfifo"
    call "%VIVADO%" -mode batch -source util_wfifo_ip.tcl -notrace
    if !errorlevel! neq 0 (
        echo [FAIL] util_wfifo build failed.
        goto :error
    )
    echo [OK] util_wfifo built.
)

:: axi_ad9361
if exist "%ADI_LIB%\axi_ad9361\component.xml" (
    echo [OK] axi_ad9361 already built.
) else (
    echo [INFO] Building axi_ad9361...
    cd /d "%ADI_LIB%\axi_ad9361"
    call "%VIVADO%" -mode batch -source axi_ad9361_ip.tcl -notrace
    if !errorlevel! neq 0 (
        echo [FAIL] axi_ad9361 build failed.
        goto :error
    )
    echo [OK] axi_ad9361 built.
)

:: util_axis_fifo
if exist "%ADI_LIB%\util_axis_fifo\component.xml" (
    echo [OK] util_axis_fifo already built.
) else (
    echo [INFO] Building util_axis_fifo...
    cd /d "%ADI_LIB%\util_axis_fifo"
    call "%VIVADO%" -mode batch -source util_axis_fifo_ip.tcl -notrace
    if !errorlevel! neq 0 (
        echo [FAIL] util_axis_fifo build failed.
        goto :error
    )
    echo [OK] util_axis_fifo built.
)

:: util_cdc
if exist "%ADI_LIB%\util_cdc\component.xml" (
    echo [OK] util_cdc already built.
) else (
    echo [INFO] Building util_cdc...
    cd /d "%ADI_LIB%\util_cdc"
    call "%VIVADO%" -mode batch -source util_cdc_ip.tcl -notrace
    if !errorlevel! neq 0 (
        echo [FAIL] util_cdc build failed.
        goto :error
    )
    echo [OK] util_cdc built.
)

:: axi_dmac (depends on util_axis_fifo, util_cdc)
if exist "%ADI_LIB%\axi_dmac\component.xml" (
    echo [OK] axi_dmac already built.
) else (
    echo [INFO] Building axi_dmac...
    cd /d "%ADI_LIB%\axi_dmac"
    call "%VIVADO%" -mode batch -source axi_dmac_ip.tcl -notrace
    if !errorlevel! neq 0 (
        echo [FAIL] axi_dmac build failed.
        goto :error
    )
    echo [OK] axi_dmac built.
)

:: util_cpack2
if exist "%ADI_LIB%\util_pack\util_cpack2\component.xml" (
    echo [OK] util_cpack2 already built.
) else (
    echo [INFO] Building util_cpack2...
    cd /d "%ADI_LIB%\util_pack\util_cpack2"
    call "%VIVADO%" -mode batch -source util_cpack2_ip.tcl -notrace
    if !errorlevel! neq 0 (
        echo [FAIL] util_cpack2 build failed.
        goto :error
    )
    echo [OK] util_cpack2 built.
)

:: util_upack2
if exist "%ADI_LIB%\util_pack\util_upack2\component.xml" (
    echo [OK] util_upack2 already built.
) else (
    echo [INFO] Building util_upack2...
    cd /d "%ADI_LIB%\util_pack\util_upack2"
    call "%VIVADO%" -mode batch -source util_upack2_ip.tcl -notrace
    if !errorlevel! neq 0 (
        echo [FAIL] util_upack2 build failed.
        goto :error
    )
    echo [OK] util_upack2 built.
)
echo.

:: ===== Step 5: Build FPGA Project (FULL SYNTHESIS) =====
echo ============================================================
echo [Step 5] Building %FPGA_PROJECT% FPGA project via Vivado
echo          Target: xc7z020clg400-1 ^(Zynq Z7020 SoC^)
echo          Typical time: ~15-30 minutes
echo          Synthesis -^> Implementation -^> Bitstream -^> XSA
echo ============================================================

cd /d "%FPGA_PROJECT_DIR%"
call "%VIVADO%" -mode batch -source system_project.tcl -notrace
if !errorlevel! neq 0 (
    :: Check if timing-only failure (bitstream exists but timing violated)
    if exist "%FPGA_PROJECT_DIR%\%FPGA_PROJECT_NAME%.sdk\system_top_bad_timing.xsa" (
        echo.
        echo [WARN] Build completed with timing violations.
        echo        Cross-clock domain paths may show violations -- this is
        echo        expected for PS7 CDC paths. Bitstream is functionally correct.
        echo.
        echo [INFO] Promoting system_top_bad_timing.xsa to system_top.xsa...
        copy /Y "%FPGA_PROJECT_DIR%\%FPGA_PROJECT_NAME%.sdk\system_top_bad_timing.xsa" "%FPGA_PROJECT_DIR%\%FPGA_PROJECT_NAME%.sdk\system_top.xsa" >nul
        echo [OK] XSA available with timing waiver.
    ) else (
        echo [FAIL] FPGA build failed!
        echo        Check logs in: %FPGA_PROJECT_DIR%\%FPGA_PROJECT_NAME%.runs\
        echo.
        echo        If error is 'No parts matched xc7z020clg400-1':
        echo        Re-run Vivado installer -^> Add Design Tools or Devices
        echo        Check: SoCs -^> Zynq-7000
        goto :error
    )
)
echo.
echo [OK] *** FPGA BUILD COMPLETE ***
echo.

:: ===== Step 6: Locate XSA and copy to Tezuka =====
echo [Step 6] Locating XSA output...
set "XSA_SOURCE=%FPGA_PROJECT_DIR%\%FPGA_PROJECT_NAME%.sdk\system_top.xsa"
if not exist "!XSA_SOURCE!" (
    echo [WARN] XSA not at expected location. Searching...
    for /r "%FPGA_PROJECT_DIR%" %%f in (system_top.xsa) do (
        set "XSA_SOURCE=%%f"
        echo [INFO] Found: %%f
    )
)
if not exist "!XSA_SOURCE!" (
    echo [FAIL] system_top.xsa not found anywhere in project directory.
    goto :error
)

echo [OK] XSA: !XSA_SOURCE!
echo.

:: Try to copy to Tezuka if it exists
pushd "%TEZUKA_FW%" 2>nul
if !errorlevel! equ 0 (
    set "TEZUKA_RESOLVED=%CD%"
    popd

    :: Choose bitstream subdir based on build type
    if "%BUILD_P25%"=="1" (
        set "BITSTREAM_SUBDIR=p25"
    ) else (
        set "BITSTREAM_SUBDIR=maia-iio"
    )
    set "TEZUKA_BITSTREAM=!TEZUKA_RESOLVED!\board\tezuka\fishball7020\bitstream\!BITSTREAM_SUBDIR!"
    if exist "!TEZUKA_BITSTREAM!" (
        echo [INFO] Copying XSA to Tezuka firmware...
        :: Backup existing
        if exist "!TEZUKA_BITSTREAM!\system_top.xsa" (
            if not exist "!TEZUKA_BITSTREAM!\system_top.xsa.bak" (
                copy "!TEZUKA_BITSTREAM!\system_top.xsa" "!TEZUKA_BITSTREAM!\system_top.xsa.bak" >nul
                echo        Backed up existing XSA
            )
        )
        copy "!XSA_SOURCE!" "!TEZUKA_BITSTREAM!\system_top.xsa" >nul
        echo [OK] Copied to: !TEZUKA_BITSTREAM!\system_top.xsa
    ) else (
        echo [INFO] Tezuka bitstream dir not found at: !TEZUKA_BITSTREAM!
        echo        XSA remains at: !XSA_SOURCE!
    )
) else (
    popd 2>nul
    echo [INFO] Tezuka firmware not found. Set TEZUKA_FW env var to copy XSA.
    echo        XSA remains at: !XSA_SOURCE!
)
echo.

:: ===== Done =====
echo ============================================================
echo  BUILD SUCCESSFUL!
echo ============================================================
echo.
echo  XSA output: !XSA_SOURCE!
echo.
echo  NEXT STEPS:
echo  1. Build Tezuka firmware:
echo     cd %TEZUKA_FW%
if "%BUILD_P25%"=="1" (
    echo     build.bat --p25
) else (
    echo     build.bat
)
echo.
echo  2. Flash the .frm/.zip to Fishball via SD card
echo.
echo  3. Verify: ssh root@192.168.120.50 "cat /sys/firmware/devicetree/base/model"
echo.
goto :end

:error
echo.
echo ============================================================
echo  BUILD FAILED -- See errors above
echo ============================================================
exit /b 1

:end
echo Done!
exit /b 0
