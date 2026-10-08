@echo off
rem Start an FH1 multiplayer server from the last release build. No world, no physics, no GPU.
rem   server.bat                         uses server\server.cfg if it exists, else an open colorado server on UDP 7777
rem   server.bat --config server\x.cfg   another config (see server\server.cfg.example)
rem   server.bat --registry              the server list the in-game browser reads (UDP 7700)
rem Linux servers: server\server.sh (pack / setup / run), docs\MULTIPLAYER.md.
rem Runs a copy of the exe so a rebuild is not blocked by this process.
setlocal
set "ROOT=%~dp0"
set "SRC=%ROOT%target\release\fh1-server.exe"
set "RUN=%TEMP%\fh1-server-%RANDOM%"

if not exist "%SRC%" (
    echo No server build found at %SRC%
    echo Build it first:  tools\cargo-locked.ps1 build --release -p fh1-net --bin fh1-server
    pause
    exit /b 1
)

if not exist "%RUN%" mkdir "%RUN%"
copy /y "%SRC%" "%RUN%\fh1-server.exe" >nul || (
    echo Could not copy the server.
    pause
    exit /b 1
)

cd /d "%ROOT%"
echo Joins and leaves print here. Console: list ^| kick ^<id^> ^| ban ^<id^> ^| quit
if not "%~1"=="" (
    "%RUN%\fh1-server.exe" %*
) else if exist "%ROOT%server\server.cfg" (
    "%RUN%\fh1-server.exe" --config "%ROOT%server\server.cfg"
) else (
    "%RUN%\fh1-server.exe" --bind 0.0.0.0:7777
)
if errorlevel 1 (
    echo The server exited with an error. The port may already be in use.
    pause
)
