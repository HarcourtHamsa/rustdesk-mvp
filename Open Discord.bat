@echo off
:: Opens Discord: launches the installed desktop app the same way its own
:: Start Menu shortcut does. Falls back to Discord's web app in your
:: default browser if it isn't installed at the standard location.

set "DISCORD_UPDATER=%LOCALAPPDATA%\Discord\Update.exe"

if exist "%DISCORD_UPDATER%" (
    start "" "%DISCORD_UPDATER%" --processStart Discord.exe
) else (
    start "" "https://discord.com/app"
)
