@echo off
rem GateForge gateway service — starts the Rust gateway on the configured port.
cd /d "%~dp0"
set PORT=20230
set UPSTREAM_BASE_URL=http://localhost:20127
rem set UPSTREAM_API_KEY=sk-...
target\debug\gateforge.exe
