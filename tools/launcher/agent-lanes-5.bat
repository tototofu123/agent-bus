@echo off
REM Five agent lanes in one window:
REM   MAIN 12pt | SUB 8pt | SUB 8pt | SUB 8pt | SUB 8pt
REM
REM How a pane gets a different size
REM -------------------------------
REM A pane cannot have its own font size. Each pane runs a *profile*, and font
REM size is a per-profile setting, so a pane's size comes from the profile it
REM runs. agent-main is 12pt, agent-sub is 8pt.
REM
REM Prerequisites
REM --------------
REM   1. pwsh -File setup-terminal-profiles.ps1
REM   2. RESTART Windows Terminal. An already-open window will not load a new
REM      profile, and every split then fails with 0x80070002.
REM
REM Why this exact syntax
REM --------------------
REM Nested subcommands are separated by a BARE semicolon. Two things break it,
REM both observed here rather than assumed:
REM
REM   - Quoting the semicolon (";") makes wt treat `split-pane ...` as a command
REM     to launch, failing with 0x80070002 "cannot find the file specified".
REM   - Passing the line through PowerShell's Start-Process -ArgumentList
REM     mangles it the same way. Run this .bat, or hand the line to cmd.
REM
REM -s / --size is also unreliable here and is omitted; resize with Alt+Shift
REM and the arrow keys.

setlocal
set WT=%LOCALAPPDATA%\Microsoft\WindowsApps\wt.exe

"%WT%" -w new -p agent-main ; split-pane -p agent-sub ; split-pane -p agent-sub ; split-pane -p agent-sub ; split-pane -p agent-sub

endlocal