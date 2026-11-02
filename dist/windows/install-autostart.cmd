@echo off
rem Start glidekm-server at logon with highest privileges (so it also works
rem over elevated windows such as Task Manager). Run this once as administrator.
rem (The settings page also has a simpler "Start with Windows" option.)
set DIR=%~dp0
schtasks /Create /TN "glidekm-server" /TR "\"%DIR%glidekm-server.exe\"" /SC ONLOGON /RL HIGHEST /F
if errorlevel 1 (
  echo Failed. Right-click this file and choose "Run as administrator".
  pause
  exit /b 1
)
schtasks /Run /TN "glidekm-server"
echo glidekm-server will now start automatically at logon.
pause
