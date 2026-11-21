@echo off
REM 构建 Windows 7 可运行的 client-go.exe。
REM 注意：Go 1.21 起要求 Windows 10+，Windows 7 只能用 Go 1.20.x 构建。
setlocal
cd /d "%~dp0"

where go >nul 2>nul
if errorlevel 1 (
  echo 找不到 go，请把 Go 1.20.x 的 bin 目录加入 PATH。
  exit /b 1
)

for /f "tokens=3" %%v in ('go version') do set "GOVER=%%v"
echo 当前工具链: %GOVER%
echo %GOVER% | findstr /c:"go1.20" >nul
if errorlevel 1 (
  echo 警告: Windows 7 只支持用 Go 1.20.x 构建（Go 1.21 起要求 Windows 10+），已中止。
  exit /b 1
)

set CGO_ENABLED=0
set GOOS=windows
set GOARCH=amd64
set GOFLAGS=-mod=mod

go build -trimpath -ldflags "-s -w" -o client-go.exe .
if errorlevel 1 (
  echo 构建失败。
  exit /b 1
)

echo 已生成 %CD%\client-go.exe
