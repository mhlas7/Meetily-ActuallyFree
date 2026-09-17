@echo off
setlocal
rem Run from the workspace root. This does not use the app's CUDA/build helper.
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul
if errorlevel 1 exit /b %errorlevel%
set "CARGO_TARGET_DIR=%~dp0target"
cargo %*
exit /b %errorlevel%
