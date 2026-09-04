@echo off
chcp 65001 >nul
setlocal enabledelayedexpansion

REM ═══════════════════════════════════════════════
REM  发票酱 — 一键发版脚本
REM  作用: 同步版本号 → 提交 → 打 tag → 推送 → 触发 GitHub Actions 构建并发布 Release
REM  用法: release.bat 2.5.2
REM        release.bat 2.5.2 "修复拖拽排序"
REM ═══════════════════════════════════════════════

set "V=%~1"
set "MSG=%~2"

if "%V%"=="" (
    echo [错误] 缺少版本号
    echo.
    echo   用法: release.bat ^<版本号^> ["提交说明"]
    echo   示例: release.bat 2.5.2
    echo   示例: release.bat 2.5.2 "修复拖拽排序"
    echo.
    exit /b 1
)

if "%MSG%"=="" set "MSG=release v%V%"

echo.
echo ═══════════════════════════════════════════
echo   发票酱 发版 v%V%
echo ═══════════════════════════════════════════
echo.

REM ── 1/5 前置检查 ────────────────────────────
echo [1/5] 检查工作区状态...

git rev-parse --git-dir >nul 2>&1
if errorlevel 1 (
    echo   [错误] 当前目录不是 Git 仓库
    exit /b 1
)

git diff --quiet HEAD 2>nul
if errorlevel 1 (
    echo   [警告] 工作区有未提交的改动，将一并提交
)

where npm >nul 2>&1
if errorlevel 1 (
    echo   [错误] 未找到 npm，请先安装 Node.js
    exit /b 1
)

REM ── 2/5 同步版本号 ──────────────────────────
echo [2/5] 同步版本号到 package.json / Cargo.toml / tauri.conf.json ...
call npm run bump %V%
if errorlevel 1 (
    echo   [错误] 版本号同步失败
    exit /b 1
)

REM ── 3/5 提交 ────────────────────────────────
echo.
echo [3/5] 提交改动 ...
git add -A
git commit -m "chore: release v%V% - %MSG%"
if errorlevel 1 (
    echo   [提示] 没有改动需要提交，继续打 tag
)

REM ── 4/5 打 tag ──────────────────────────────
echo.
echo [4/5] 创建 tag v%V% ...
git tag -d v%V% >nul 2>&1
git tag v%V%
if errorlevel 1 (
    echo   [错误] 创建 tag 失败
    exit /b 1
)

REM ── 5/5 推送 ────────────────────────────────
echo.
echo [5/5] 推送到 GitHub ...
git push origin master
if errorlevel 1 (
    echo   [错误] 推送 master 失败
    exit /b 1
)

git push origin v%V% --force
if errorlevel 1 (
    echo   [错误] 推送 tag 失败
    exit /b 1
)

echo.
echo ═══════════════════════════════════════════
echo   已推送 tag v%V%，GitHub Actions 开始构建
echo.
echo   构建进度: https://github.com/chenghaitao/fapiao-print/actions
echo   发布页面: https://github.com/chenghaitao/fapiao-print/releases
echo.
echo   预计 40-120 分钟（OCR 版需编译 MNN）
echo ═══════════════════════════════════════════
echo.

endlocal
