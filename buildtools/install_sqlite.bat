@rem Control variables
@rem - SQLite {Note: `powershell` cannot `expand-archive` to `C:\Program Files (x86)`}
@set sqlite_zip=sqlite-dll-win64-x64-3310100.zip
@set sqlite_folder=%USERPROFILE%\.sqlite
@set sqlite_runtime=sqlite3.dll

@echo Downloading and installing SQLite...
@echo.

@rem Determine if running as administrator
@call :TEST_ADMINISTRATOR
@if [%errorlevel%]==[10101] goto :END

@rem Install dependencies
@call :INSTALL_SQLITE
@goto END:
`
:INSTALL_SQLITE
@rem Download install file
@powershell wget https://www.sqlite.org/2020/%sqlite_zip% -outfile "%TEMP%\%sqlite_zip%"
@rem Install
@powershell expand-archive -Force -LiteralPath "%TEMP%\%sqlite_zip%" -DestinationPath "%sqlite_folder%"
@rem Set Tari environment variables
@set TARI_SQLITE_DIR=%sqlite_folder%
@setx TARI_SQLITE_DIR %TARI_SQLITE_DIR%
@setx /m USERNAME %USERNAME%
@rem Test installation
@if not exist "%TARI_SQLITE_DIR%\%sqlite_runtime%" (
@echo.
    @echo.
    @echo Problem with SQLite installation, "%sqlite_runtime%" not found!
    @echo {Please try installing this dependency using the manual procedure described in the README file.}
    @echo.
    @pause
) else (
    @echo.
    @echo SQLite installation found at "%TARI_SQLITE_DIR%"
)
@goto :eof

:TEST_ADMINISTRATOR
@echo.
@set guid=%random%%random%-%random%-%random%-%random%-%random%%random%%random%
@mkdir %WINDIR%\%guid%>nul 2>&1
@rmdir %WINDIR%\%guid%>nul 2>&1
@if %ERRORLEVEL% equ 0 (
    @echo Administrator OK
    @echo.
) else (
    @echo Please run as administrator {hint: Right click, then "Run as administrator"}
    @echo.
    @exit /b 10101
)
@goto :eof

:END
@echo.
@if not [%1]==[NO_PAUSE] (
    @pause
) else (
    @ping -n 3 localhost>nul
)
@@if [%errorlevel%]==[10101] exit
