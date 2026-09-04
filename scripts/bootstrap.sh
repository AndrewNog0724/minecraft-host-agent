#!/usr/bin/env bash
# 环境引导脚本（归档主线 D13 恢复，v2 适配）：Linux/macOS 一键搭好 Rust
# 工具链并安装本应用。Windows 请用 scripts/bootstrap-windows.ps1。
#
# 用法：
#   bash scripts/bootstrap.sh
#
# 幂等性：每一步先检测后安装，重复执行无副作用。

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

echo "=== Minecraft Host Agent (MCHA) 环境引导（Linux/macOS）==="
echo "仓库：$REPO_ROOT"

# ---- 1. C/C++ 链接器（Rust 链接阶段依赖系统 cc） ----
if command -v cc >/dev/null 2>&1 || command -v clang >/dev/null 2>&1; then
    echo "[ok] 系统链接器已就绪"
else
    echo "[!] 未检测到 cc/clang。请先安装编译工具链后重跑："
    echo "    Debian/Ubuntu：sudo apt install build-essential"
    echo "    Fedora：        sudo dnf install gcc"
    echo "    macOS：         xcode-select --install"
    exit 1
fi

# ---- 2. rustup / cargo ----
if command -v cargo >/dev/null 2>&1; then
    echo "[ok] Rust 工具链已安装：$(cargo --version)"
else
    if [ -x "$HOME/.cargo/bin/cargo" ]; then
        export PATH="$HOME/.cargo/bin:$PATH"
        echo "[ok] Rust 工具链已安装（已补 PATH）：$(cargo --version)"
    else
        echo "[..] 正在安装 rustup（官方脚本，用户级安装，无需 root）..."
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
        export PATH="$HOME/.cargo/bin:$PATH"
        echo "[ok] rustup 安装完成：$(cargo --version)"
    fi
fi

# ---- 3. 编译并安装本应用到 ~/.cargo/bin（天然在 PATH） ----
echo "[..] 正在编译并安装 mcha（cargo install --path .，首次约几分钟）..."
(cd "$REPO_ROOT" && cargo install --path .)

# ---- 4. 验证与下一步 ----
if command -v mcha >/dev/null 2>&1; then
    echo "[ok] mcha 已可在任意目录直接调用：$(command -v mcha)"
else
    echo "[!] mcha 已装进 ~/.cargo/bin，但当前会话 PATH 未包含它。"
    echo "    新开一个终端窗口即可生效；或执行：export PATH=\"\$HOME/.cargo/bin:\$PATH\""
fi

echo
echo "=== 下一步 ==="
echo "  mcha setup   —— 交互式向导完成模型配置与工作区设定（必填仅 3 项）"
echo "  mcha         —— 运行 Minecraft Host Agent（MCHA）"
