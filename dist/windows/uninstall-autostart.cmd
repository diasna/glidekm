@echo off
schtasks /End /TN "glidekm-server" >nul 2>&1
schtasks /Delete /TN "glidekm-server" /F
taskkill /IM glidekm-server.exe /F >nul 2>&1
pause
