@echo off
REM ThoughtGraph Python web edition launcher for Windows.
REM Tries the `py` launcher first (ships with Python.org installer),
REM then falls back to `python` on PATH.

setlocal
cd /d "%~dp0"

where py >nul 2>nul
if %ERRORLEVEL%==0 (
    py -3 server.py %*
    goto :eof
)

where python >nul 2>nul
if %ERRORLEVEL%==0 (
    python server.py %*
    goto :eof
)

echo Python 3 was not found on PATH.
echo Install it from https://www.python.org/downloads/ ^(check "Add Python to PATH"^),
echo or from the Microsoft Store ^(search "Python 3"^).
pause
exit /b 1
