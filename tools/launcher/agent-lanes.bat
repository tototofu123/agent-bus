@echo off
REM Agent lanes: one window, three panes.
REM   pane 1  MAIN     12pt  the main agent
REM   pane 2  SUB       8pt  subagents
REM   pane 3  REPORT    8pt  bus watch (report lane)
REM
REM Panes get their font size from their Windows Terminal profile, so the size
REM difference comes from the profile each pane runs, not from the window.
REM The agent-main / agent-sub / agent-report profiles are written into
REM settings.json by setup-terminal-profiles.ps1.
REM
REM Nested splits are separated by semicolons in the wt CLI. Quoting them is
REM what makes this work from a .bat.

setlocal
set WT=%LOCALAPPDATA%\Microsoft\WindowsApps\wt.exe
set BUS=%LOCALAPPDATA%\agent-bus\bin\bus.exe
set REPO=%USERPROFILE%\Documents\antigravity_projects\doing-projects\agent-bus
set SHIM=%REPO%\shims\omp\agent-bus.ts
set ROOM=%AGENT_BUS_ROOM%
if "%ROOM%"=="" set ROOM=#repo:agent-bus

"%WT%" -w new --size 200x60 -p "agent-main" ^
  ";" split-pane -p "agent-sub" -s 34% ^
  ";" split-pane -p "agent-report" -s 50%
endlocal