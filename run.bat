@echo off
rem Builds zed-lite with the "release-local" profile (optimized, incremental, no LTO) and runs it.
rem Arguments are passed on to zed, e.g. `run.bat D:\some\project`.
rem
rem ZED_LITE_DATA_DIR   where settings and state are kept (default: %LOCALAPPDATA%\zed-lite).
rem                     Kept apart from an installed Zed, whose settings zed-lite cannot read.
rem ZED_LITE_CARGO_ARGS extra arguments for `cargo build`.
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

cargo build --profile release-local --package zed %ZED_LITE_CARGO_ARGS%
if errorlevel 1 (
    echo.
    echo Build failed.
    exit /b 1
)

start "" "%~dp0target\release-local\zed.exe" --user-data-dir "%ZED_LITE_DATA_DIR%" %*
