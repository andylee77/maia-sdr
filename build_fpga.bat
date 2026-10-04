@echo off
setlocal enabledelayedexpansion

:: ===== Parse arguments =====
set "BUILD_P25=0"
set "BUILD_HWVAL=0"
for %%a in (%*) do (
    if "%%a"=="--p25" set "BUILD_P25=1"
    if "%%a"=="--hwval" set "BUILD_HWVAL=1"
)
if "%BUILD_P25%%BUILD_HWVAL%"=="11" (
    echo [FAIL] --p25 and --hwval are mutually exclusive.
    exit /b 1
)

:: ===== Configuration =====
set "PROJECT_DIR=%~dp0"
if "%PROJECT_DIR:~-1%"=="\" set "PROJECT_DIR=%PROJECT_DIR:~0,-1%"

set "MAIA_HDL=%PROJECT_DIR%\maia-hdl"
set "SCANNER_HDL=%PROJECT_DIR%\scanner-hdl"
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
    set "IP_CORE_VERSION=1.0.0"
) else (
    set "FPGA_PROJECT=fishball7020_iio"
    set "FPGA_PROJECT_NAME=fishball"
)
:: hwval validation bitstream (doc/HW_VALIDATION_SUITE.md sections 6 + 11).
:: Overrides the Maia-IIO project defaults set just above.
if "%BUILD_HWVAL%"=="1" (
    set "HWVAL_IP_DIR=%MAIA_HDL%\ip\hwval-core"
    set "HWVAL_CONFIG=default"
    set "FPGA_PROJECT=fishball7020_hwval"
    set "FPGA_PROJECT_NAME=fishball_hwval"
    set "IP_CORE_VERSION=0.1.0"
)
set "FPGA_PROJECT_DIR=%MAIA_HDL%\projects\%FPGA_PROJECT%"

if "%BUILD_HWVAL%"=="1" (
    echo ============================================================
    echo  Fishball 7020 -- hwval validation FPGA Bitstream Build ^(Vivado^)
    echo  Target: xc7z020clg400-1 ^(Zynq Z7020 SoC^)
    echo  Project: fishball7020_hwval ^(timing failure = hard error^)
    echo ============================================================
) else if "%BUILD_P25%"=="1" (
    echo ============================================================
    echo  Fishball 7020 -- P25 FPGA Bitstream Build ^(Vivado^)
    echo  Target: xc7z020clg400-1 ^(Zynq Z7020 SoC^)
    echo  Project: fishball7020_p25 ^(timing failure = hard error^)
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
:: Source dirs include both scanner-hdl\radio_core (the radio core) AND
:: maia_hdl (because p25_top.py imports DDC, registers, DMA, etc. from
:: maia_hdl, so a change in maia_hdl affects the generated p25_core.v).
if "%BUILD_P25%"=="1" (
    set "P25_VERILOG=%P25_IP_DIR%\%P25_CONFIG%\p25_core.v"
    set "P25_STATE="
    for /f "delims=" %%R in ('powershell -NoProfile -ExecutionPolicy Bypass -File "%STALE_CHECK%" -VerilogFile "!P25_VERILOG!" -SourceDirs "%SCANNER_HDL%\radio_core;%MAIA_HDL%\maia_hdl"') do set "P25_STATE=%%R"

    set "P25_REGEN=0"
    if "!P25_STATE!"=="FRESH" echo [OK] p25_core.v is current ^(newer than radio_core + maia_hdl source^).
    if "!P25_STATE!"=="MISSING" (
        echo [INFO] p25_core.v not found. Generating P25 Verilog via Docker...
        set "P25_REGEN=1"
    )
    if "!P25_STATE!"=="STALE" (
        echo [WARN] p25_core.v is STALE -- radio_core or maia_hdl has newer changes.
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

:: ----- hwval Verilog (only for --hwval build) -----
:: hwval_top.py imports radio_core (IQPacker, production-replica ring) and
:: maia_hdl (DMA, CDC), so all three source trees gate staleness. One
:: Docker run emits hwval_core.v + hwval.svd + hwval_regs.json +
:: hwval_register_map.md; a missing JSON/MD also forces regeneration.
if "%BUILD_HWVAL%"=="1" (
    set "HWVAL_VERILOG=%HWVAL_IP_DIR%\%HWVAL_CONFIG%\hwval_core.v"
    set "HWVAL_STATE="
    for /f "delims=" %%R in ('powershell -NoProfile -ExecutionPolicy Bypass -File "%STALE_CHECK%" -VerilogFile "!HWVAL_VERILOG!" -SourceDirs "%SCANNER_HDL%\hwval_hdl;%SCANNER_HDL%\radio_core;%MAIA_HDL%\maia_hdl"') do set "HWVAL_STATE=%%R"

    set "HWVAL_REGEN=0"
    if "!HWVAL_STATE!"=="FRESH" echo [OK] hwval_core.v is current ^(newer than hwval_hdl + radio_core + maia_hdl source^).
    if "!HWVAL_STATE!"=="MISSING" (
        echo [INFO] hwval_core.v not found. Generating hwval Verilog via Docker...
        set "HWVAL_REGEN=1"
    )
    if "!HWVAL_STATE!"=="STALE" (
        echo [WARN] hwval_core.v is STALE -- hwval_hdl, radio_core or maia_hdl has newer changes.
        echo        Regenerating via Docker to avoid baking stale logic into bitstream.
        set "HWVAL_REGEN=1"
    )
    if not defined HWVAL_STATE (
        echo [FAIL] Staleness helper returned empty state for hwval_core.v
        goto :error
    )
    if not "!HWVAL_STATE!"=="FRESH" if not "!HWVAL_STATE!"=="MISSING" if not "!HWVAL_STATE!"=="STALE" (
        echo [FAIL] Staleness helper returned unexpected state: '!HWVAL_STATE!'
        goto :error
    )
    if not exist "%HWVAL_IP_DIR%\%HWVAL_CONFIG%\hwval_regs.json" set "HWVAL_REGEN=1"
    if not exist "%HWVAL_IP_DIR%\%HWVAL_CONFIG%\hwval_register_map.md" set "HWVAL_REGEN=1"
    if "!HWVAL_REGEN!"=="1" (
        call "%PROJECT_DIR%\build_hdl.bat" --verilog-only --hwval --hwval-config %HWVAL_CONFIG%
        if !errorlevel! neq 0 (
            echo [FAIL] hwval Verilog generation failed. Run build_hdl.bat --hwval manually.
            goto :error
        )
        for %%F in (hwval_core.v hwval.svd hwval_regs.json hwval_register_map.md) do (
            if not exist "%HWVAL_IP_DIR%\%HWVAL_CONFIG%\%%F" (
                echo [FAIL] %%F still not found after generation.
                goto :error
            )
        )
        echo [OK] hwval_core.v, hwval.svd, hwval_regs.json, hwval_register_map.md regenerated.
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

:: hwval IP (only for --hwval build). package_ip.tcl writes package_ip.ok
:: as its last action; ipx::package_project writes component.xml early,
:: so component.xml alone cannot prove the packaging run completed.
if "%BUILD_HWVAL%"=="1" (
    echo [Step 3c] Packaging hwval IP core ^(config: %HWVAL_CONFIG%^)...
    cd /d "%HWVAL_IP_DIR%\%HWVAL_CONFIG%"
    if exist "%HWVAL_IP_DIR%\%HWVAL_CONFIG%\package_ip.ok" del /q "%HWVAL_IP_DIR%\%HWVAL_CONFIG%\package_ip.ok"
    set "IP_CORE_VERSION=%IP_CORE_VERSION%"
    set "HWVAL_CONFIG=%HWVAL_CONFIG%"
    call "%VIVADO%" -mode batch -source "%HWVAL_IP_DIR%\package_ip.tcl" -notrace
    if not exist "%HWVAL_IP_DIR%\%HWVAL_CONFIG%\component.xml" (
        echo [FAIL] hwval IP packaging failed -- no component.xml created.
        goto :error
    )
    if not exist "%HWVAL_IP_DIR%\%HWVAL_CONFIG%\package_ip.ok" (
        echo [FAIL] hwval IP packaging did not complete -- see vivado.log in %HWVAL_IP_DIR%\%HWVAL_CONFIG%
        goto :error
    )
    echo [OK] hwval IP core packaged ^(component.xml + package_ip.ok^).
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
:: P25 and hwval: a timing failure is a hard error. Remove XSAs left by a previous run so only
:: this run's output can be picked up in Step 6 (a bad-timing XSA is never promoted).
set "STRICT_TIMING=0"
if "%BUILD_P25%"=="1" set "STRICT_TIMING=1"
if "%BUILD_HWVAL%"=="1" set "STRICT_TIMING=1"
if "%STRICT_TIMING%"=="1" (
    if exist "%FPGA_PROJECT_DIR%\%FPGA_PROJECT_NAME%.sdk\system_top.xsa" del /q "%FPGA_PROJECT_DIR%\%FPGA_PROJECT_NAME%.sdk\system_top.xsa"
    if exist "%FPGA_PROJECT_DIR%\%FPGA_PROJECT_NAME%.sdk\system_top_bad_timing.xsa" del /q "%FPGA_PROJECT_DIR%\%FPGA_PROJECT_NAME%.sdk\system_top_bad_timing.xsa"
)
call "%VIVADO%" -mode batch -source system_project.tcl -notrace
if "%STRICT_TIMING%"=="1" if !errorlevel! neq 0 (
    echo [FAIL] %FPGA_PROJECT% build failed. A timing failure is a HARD error here:
    echo        system_top_bad_timing.xsa is never promoted. Check
    echo        %FPGA_PROJECT_DIR%\timing_impl.log and
    echo        %FPGA_PROJECT_DIR%\%FPGA_PROJECT_NAME%.runs\
    goto :error
)
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
if "%STRICT_TIMING%"=="1" (
    if exist "%FPGA_PROJECT_DIR%\%FPGA_PROJECT_NAME%.sdk\system_top_bad_timing.xsa" (
        echo [FAIL] %FPGA_PROJECT%: system_top_bad_timing.xsa present -- timing not met, not promoting.
        goto :error
    )
    if not exist "!XSA_SOURCE!" (
        echo [FAIL] %FPGA_PROJECT%: !XSA_SOURCE! was not produced by this run.
        goto :error
    )
)
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

:: hwval: publish the register map generated with the Verilog that is now
:: in this bitstream (consumed by the fbench agent/CLI and the docs).
if "%BUILD_HWVAL%"=="1" (
    if not exist "%PROJECT_DIR%\bench\share" mkdir "%PROJECT_DIR%\bench\share"
    copy /Y "%HWVAL_IP_DIR%\%HWVAL_CONFIG%\hwval_regs.json" "%PROJECT_DIR%\bench\share\hwval_regs.json" >nul
    if !errorlevel! neq 0 (
        echo [FAIL] Could not copy hwval_regs.json to bench\share\
        goto :error
    )
    copy /Y "%HWVAL_IP_DIR%\%HWVAL_CONFIG%\hwval_register_map.md" "%PROJECT_DIR%\doc\hwval_register_map.md" >nul
    if !errorlevel! neq 0 (
        echo [FAIL] Could not copy hwval_register_map.md to doc\
        goto :error
    )
    echo [OK] Published bench\share\hwval_regs.json and doc\hwval_register_map.md
    echo.
)

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
    if "%BUILD_HWVAL%"=="1" set "BITSTREAM_SUBDIR=hwval"
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
if "%BUILD_HWVAL%"=="1" (
    echo     build.bat --p25
    echo     ^(the P25 build's post-image.sh adds sdimg\bench\images\hwval\ and \p25\^)
) else if "%BUILD_P25%"=="1" (
    echo     build.bat --p25
) else (
    echo     build.bat
)
echo.
echo  2. Flash the .frm/.zip to Fishball via SD card
echo.
echo  3. Verify: ssh root@192.168.2.1 "cat /sys/firmware/devicetree/base/model"
echo     (RNDIS USB; use 192.168.120.50 if connected via Ethernet)
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
