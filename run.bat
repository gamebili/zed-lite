@echo off
rem Builds zed-lite with the "release-small" profile and Fat LTO, then runs it.
rem Arguments are passed on to zed, e.g. `run.bat D:\some\project`.
rem
rem ZED_LITE_DATA_DIR   where settings and state are kept (default: %LOCALAPPDATA%\zed-lite).
rem                     Kept apart from an installed Zed, whose settings zed-lite cannot read.
rem ZED_LITE_CARGO_ARGS extra Cargo arguments (release-small builds only the zed binary).
rem ZED_LITE_BUILD_PROFILE defaults to release-small; use release-local for faster rebuilds.
setlocal
cd /d "%~dp0"

where cargo >nul 2>nul
if errorlevel 1 (
    echo cargo was not found on PATH. Install Rust from https://rustup.rs first.
    exit /b 1
)
where cmake >nul 2>nul
if errorlevel 1 if exist "%ProgramFiles%\CMake\bin\cmake.exe" set "PATH=%ProgramFiles%\CMake\bin;%PATH%"

if not exist "secrets\key-wrap.hex" (
    echo warning: secrets\key-wrap.hex is missing, DeepSeek completions will not work.
)
if not exist "secrets\key.enc" (
    echo warning: secrets\key.enc is missing, DeepSeek completions will not work.
)

if not defined ZED_LITE_DATA_DIR set "ZED_LITE_DATA_DIR=%LOCALAPPDATA%\zed-lite"
if not defined ZED_LITE_BUILD_PROFILE set "ZED_LITE_BUILD_PROFILE=release-small"

if "%ZED_LITE_BUILD_PROFILE%"=="release-small" (
    rem Apply Fat LTO only to the final binary so dependency builds stay cached.
    cargo rustc --profile "%ZED_LITE_BUILD_PROFILE%" --package zed --bin zed %ZED_LITE_CARGO_ARGS% -- -C lto=fat -C embed-bitcode=yes
) else (
    cargo build --profile "%ZED_LITE_BUILD_PROFILE%" --package zed %ZED_LITE_CARGO_ARGS%
)
if errorlevel 1 (
    echo.
    echo Build failed.
    exit /b 1
)

set "ZED_LITE_OUTPUT_DIR=%ZED_LITE_BUILD_PROFILE%"
if "%ZED_LITE_BUILD_PROFILE%"=="dev" set "ZED_LITE_OUTPUT_DIR=debug"
start "" "%~dp0target\%ZED_LITE_OUTPUT_DIR%\zed.exe" --user-data-dir "%ZED_LITE_DATA_DIR%" %*
