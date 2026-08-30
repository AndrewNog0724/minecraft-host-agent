# 环境引导脚本（FR-18，决议 D13）：Windows 一键搭好 Rust 工具链并安装本应用。
#
# 用法（PowerShell，无需管理员；VS Build Tools 安装器自行请求提权）：
#   powershell -ExecutionPolicy Bypass -File scripts\bootstrap-windows.ps1
#
# 幂等性：每一步先检测后安装，重复执行无副作用。

$ErrorActionPreference = "Stop"

# 仓库根目录 = 脚本所在目录的上一级
$RepoRoot = Split-Path -Parent $PSScriptRoot
$CargoBin = Join-Path $env:USERPROFILE ".cargo\bin"

function Test-Command($Name) {
    return [bool](Get-Command $Name -ErrorAction SilentlyContinue)
}

Write-Host "=== mc-host-agent 环境引导（Windows）===" -ForegroundColor Cyan
Write-Host "仓库：$RepoRoot"

# ---- 1. winget（Win10 1709+ 自带；缺失则只能手动装，给出指引） ----
if (-not (Test-Command "winget")) {
    Write-Host "[!] 未检测到 winget（需 Windows 10 1709+ / App Installer）。" -ForegroundColor Yellow
    Write-Host "    请从微软商店安装「应用安装程序」后重跑本脚本，或手动完成第 2、3 步。"
    exit 1
}

# ---- 2. rustup / cargo ----
if (Test-Command "cargo") {
    Write-Host "[ok] Rust 工具链已安装：$((cargo --version))" -ForegroundColor Green
} elseif (Test-Path (Join-Path $CargoBin "cargo.exe")) {
    # 已装但当前会话 PATH 未刷新：直接用绝对路径并补进本会话
    $env:PATH = "$CargoBin;$env:PATH"
    Write-Host "[ok] Rust 工具链已安装（已补 PATH）：$(& (Join-Path $CargoBin 'cargo.exe') --version)" -ForegroundColor Green
} else {
    Write-Host "[..] 正在通过 winget 安装 rustup（含稳定版 MSVC 工具链）..."
    winget install --id Rustlang.Rustup -e --accept-source-agreements --accept-package-agreements
    if ($LASTEXITCODE -ne 0) { Write-Host "[!] rustup 安装失败，请手动安装：https://rustup.rs" -ForegroundColor Red; exit 1 }
    $env:PATH = "$CargoBin;$env:PATH"
    Write-Host "[ok] rustup 安装完成" -ForegroundColor Green
}

# ---- 3. MSVC 链接器（VS Build Tools 的 C++ 工作负载，提供 link.exe） ----
$vsWhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$hasMsvc = $false
if (Test-Path $vsWhere) {
    $inst = & $vsWhere -products Microsoft.VisualStudio.Product.BuildTools -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    $hasMsvc = [bool]$inst
}
if ($hasMsvc) {
    Write-Host "[ok] MSVC 链接器已就绪（VS Build Tools / C++ 工作负载）" -ForegroundColor Green
} else {
    Write-Host "[..] 正在安装 VS Build Tools（C++ 工作负载，约 2-6 GB，需要几分钟）..."
    winget install --id Microsoft.VisualStudio.2022.BuildTools -e --accept-source-agreements --accept-package-agreements `
        --override "--quiet --wait --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
    if ($LASTEXITCODE -ne 0) { Write-Host "[!] Build Tools 安装失败，请手动安装并勾选「使用 C++ 的桌面开发」" -ForegroundColor Red; exit 1 }
    Write-Host "[ok] VS Build Tools 安装完成" -ForegroundColor Green
}

# ---- 4. 编译并安装本应用到 ~/.cargo/bin（天然在 PATH） ----
Write-Host "[..] 正在编译并安装 agent（cargo install --path .，首次约几分钟）..."
Push-Location $RepoRoot
try {
    cargo install --path .
    if ($LASTEXITCODE -ne 0) { Write-Host "[!] 安装失败，请把上方报错反馈给开发者" -ForegroundColor Red; exit 1 }
} finally {
    Pop-Location
}

# ---- 5. 验证与下一步 ----
$agentCmd = Get-Command agent -ErrorAction SilentlyContinue
if ($agentCmd) {
    Write-Host "[ok] agent 已可在任意目录直接调用：$($agentCmd.Source)" -ForegroundColor Green
} else {
    Write-Host "[!] agent 已装进 $CargoBin，但当前会话 PATH 未包含它。" -ForegroundColor Yellow
    Write-Host "    新开一个终端窗口即可生效；或手动把该目录加入用户 PATH。"
}

Write-Host ""
Write-Host "=== 下一步 ===" -ForegroundColor Cyan
Write-Host "  运行  agent setup   —— 交互式向导完成模型配置与工作区设定（必填仅 3 项）"
Write-Host "  然后  agent new     —— 一句话开服"
