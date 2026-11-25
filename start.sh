#!/usr/bin/env bash
# 启动检查脚本：验证 Rust 工具链、SAP SDK、配置文件、端口，然后启动。
# 用法：./start.sh
# 适用于 Linux / macOS / Git Bash（Windows）。

set -e

# 颜色输出（非交互终端自动关色）
if [ -t 1 ]; then
    GREEN='\033[0;32m'; YELLOW='\033[0;33m'; RED='\033[0;31m'; NC='\033[0m'
else
    GREEN=''; YELLOW=''; RED=''; NC=''
fi

ok()   { printf "${GREEN}✓${NC} %s\n" "$1"; }
warn() { printf "${YELLOW}!${NC} %s\n" "$1"; }
err()  { printf "${RED}✗${NC} %s\n" "$1"; }

echo "=== sap-for-agents 启动检查 ==="
echo ""

# 单实例锁：防止多个守护循环并发启动，抢同一个端口造成
# "AddrInUse → 退出 → 1 秒后重启" 的死循环（2026-11-25 事故：3 个守护循环
# 同时运行，累计重启 13 万次，向 s4h:3300 疯狂建连）。
# 用 mkdir 的原子性做跨平台锁（macOS/Linux/Git Bash 均可用）。
# 注意：macOS 自带 bash 3.2 会吞掉「未加花括号的 $变量」后面紧跟的多字节字符的首字节，
# 本段与下方循环里的中文提示一律写 ${VAR}。
LOCK_DIR="${TMPDIR:-/tmp}/sap-for-agents.start.lock"
STALE_DIR="$LOCK_DIR.stale.$$"

# 锁持有者是否还活着。用 ps -p 而非 kill -0：后者对「进程存在但属于别的用户」同样
# 失败，分不清死活；没有 ps（精简容器）或 Git Bash（ps 不支持 -p）时退回 kill -0。
pid_alive() {
    case "$(uname -s)" in
        MINGW*|MSYS*|CYGWIN*) kill -0 "$1" 2>/dev/null ;;
        *) if command -v ps >/dev/null 2>&1; then ps -p "$1" >/dev/null 2>&1; else kill -0 "$1" 2>/dev/null; fi ;;
    esac
}

# 取锁：mkdir 抢不到时，只有确认持有者进程已死才回收残留锁。残留来自 EXIT trap 来不及
# 跑的场景（kill -9、断电重启）——macOS 的 $TMPDIR 重启后仍在，不回收的话下一次
# ./start.sh 会被一把没人持有的锁永远拒绝。
# 回收走「rename 搬走 → 独占下复核 → 重建」：两个进程同时抢同一把残留锁时，rename
# 只有一个能成功，另一方不会删掉对方刚建好的新锁。
acquire_lock() {
    mkdir "$LOCK_DIR" 2>/dev/null && return 0
    LOCK_PID="$(cat "$LOCK_DIR/pid" 2>/dev/null || true)"
    if [ -z "$LOCK_PID" ] || pid_alive "$LOCK_PID"; then
        return 1
    fi
    mv "$LOCK_DIR" "$STALE_DIR" 2>/dev/null || { LOCK_PID="$(cat "$LOCK_DIR/pid" 2>/dev/null || true)"; return 1; }
    MOVED_PID="$(cat "$STALE_DIR/pid" 2>/dev/null || true)"
    if [ -n "$MOVED_PID" ] && pid_alive "$MOVED_PID"; then
        mv "$STALE_DIR" "$LOCK_DIR" 2>/dev/null || true   # 搬走的是别人刚建的新锁：放回
        LOCK_PID="$MOVED_PID"
        return 1
    fi
    rm -rf "$STALE_DIR"
    mkdir "$LOCK_DIR" 2>/dev/null || return 1
    warn "残留锁已回收：持有者 PID ${MOVED_PID:-未知} 早已退出（kill -9 / 重启后 trap 没跑到）"
}

if ! acquire_lock; then
    err "另一个 start.sh 守护进程正在运行（锁: ${LOCK_DIR}，PID: ${LOCK_PID:-未知}）"
    echo "  如确认该进程已退出，删除锁后重试：rm -rf \"$LOCK_DIR\""
    exit 1
fi
printf '%s\n' "$$" > "$LOCK_DIR/pid"
# 只删自己的锁——pid 文件不是自己时，说明锁目录已被别的进程接管
cleanup_lock() {
    if [ "$(cat "$LOCK_DIR/pid" 2>/dev/null)" = "$$" ]; then
        rm -rf "$LOCK_DIR"
    fi
    true
}
trap cleanup_lock EXIT
ok "单实例锁已获取（PID $$）"
echo ""

# 1. Rust 工具链
if command -v cargo >/dev/null 2>&1; then
    ok "Rust 已安装: $(cargo --version)"
else
    err "未检测到 Rust 工具链"
    echo ""
    echo "请安装 Rust："
    echo "  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
    echo "  Windows 也可用：winget install Rustlang.Rustup"
    echo ""
    echo "或直接下载预编译二进制：见 README §「下载预编译二进制」"
    exit 1
fi
echo ""

# 2. 检测当前平台对应的 SDK 子目录
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)
case "$ARCH" in
    x86_64|amd64) ARCH="x86_64" ;;
    aarch64|arm64) ARCH="aarch64" ;;
esac
case "$OS" in
    mingw*|msys*|cygwin*) OS="windows" ;;
    darwin) OS="darwin" ;;
    linux) OS="linux" ;;
esac
SDK_SUBDIR="nwrfcsdk/lib/${OS}-${ARCH}"

echo "检测到平台: ${OS}-${ARCH}"

# 3. SAP SDK 检查（三选一即可通过）
SDK_OK=0

# 3a. 优先：环境变量 SAP_SDK_DIR（build.rs 也识别这个变量）
if [ -n "$SAP_SDK_DIR" ]; then
    if [ -d "$SAP_SDK_DIR/lib/${OS}-${ARCH}" ] && \
       [ -n "$(ls "$SAP_SDK_DIR/lib/${OS}-${ARCH}"/*.so "$SAP_SDK_DIR/lib/${OS}-${ARCH}"/*.dylib "$SAP_SDK_DIR/lib/${OS}-${ARCH}"/*.dll 2>/dev/null | head -1)" ]; then
        ok "SAP SDK 已就位（环境变量 SAP_SDK_DIR）: $SAP_SDK_DIR/lib/${OS}-${ARCH}"
        SDK_OK=1
    else
        warn "环境变量 SAP_SDK_DIR='$SAP_SDK_DIR' 已设置但其下 lib/${OS}-${ARCH}/ 缺少库文件，忽略"
    fi
fi

# 3b. 默认：nwrfcsdk/lib/<os>-<arch>/ 已有库文件
if [ "$SDK_OK" = "0" ] && [ -d "$SDK_SUBDIR" ]; then
    LIB_COUNT=$(find "$SDK_SUBDIR" -maxdepth 1 \( -name "*.dll" -o -name "*.so" -o -name "*.dylib" \) 2>/dev/null | wc -l)
    if [ "$LIB_COUNT" -gt 0 ]; then
        ok "SAP SDK 已就位: $SDK_SUBDIR ($LIB_COUNT 个库文件)"
        SDK_OK=1
    fi
fi

# 3c. 自动解压 nwrfcsdk/lib/*/*.zip
#     用户的常见痛点：下载了 NWRFC SDK zip（如 nwrfcsdk-7.50.18-linux-x86_64.zip）
#     但不知道文件应该放哪。脚本自动识别 + 解压到正确目录。
if [ "$SDK_OK" = "0" ]; then
    ZIP_FILE=$(find nwrfcsdk/lib -maxdepth 2 -name "nwrfcsdk-*.zip" 2>/dev/null | head -1 || true)
    if [ -n "$ZIP_FILE" ]; then
        echo ""
        echo "发现 SDK 安装包: $ZIP_FILE"
        echo "自动解压到 nwrfcsdk/lib/${OS}-${ARCH}/ ..."
        EXTRACT_TMP=$(mktemp -d)
        if command -v unzip >/dev/null 2>&1; then
            unzip -q "$ZIP_FILE" -d "$EXTRACT_TMP"
        else
            err "需要 unzip 命令来解压 SDK（macOS 默认无，可 brew install unzip）"
            rm -rf "$EXTRACT_TMP"
            exit 1
        fi

        # SAP SDK zip 通常结构：nwrfcsdk/lib/<file>（无平台子目录）
        # 或者：nwrfcsdk/<os>-<arch>/lib/<file>
        # 按当前平台筛选库文件扩展名，避免把错误平台的 SDK 复制进来
        case "$OS" in
            darwin) LIB_EXT="dylib" ;;
            linux)  LIB_EXT="so" ;;
            *)      LIB_EXT="dll" ;;
        esac
        mkdir -p "$SDK_SUBDIR"
        SRC_LIB_DIR=$(find "$EXTRACT_TMP" -type d -name "lib" -path "*nwrfcsdk*" 2>/dev/null | head -1 || true)
        if [ -n "$SRC_LIB_DIR" ]; then
            cp -f "$SRC_LIB_DIR"/*."$LIB_EXT" "$SDK_SUBDIR/" 2>/dev/null || true
        else
            # 兜底：扫描整个 zip 找当前平台的库文件
            find "$EXTRACT_TMP" -type f -name "*.$LIB_EXT" \
                -exec cp -f {} "$SDK_SUBDIR/" \; 2>/dev/null || true
        fi
        # 校验：若 zip 里没有任何当前平台的库文件，多半是放错平台了
        FOUND_COUNT=$(find "$EXTRACT_TMP" -type f -name "*.$LIB_EXT" 2>/dev/null | wc -l)
        rm -rf "$EXTRACT_TMP"
        if [ "$FOUND_COUNT" -eq 0 ]; then
            echo ""
            err "zip 中未找到当前平台 ($OS-$ARCH) 的库文件 (*.$LIB_EXT)"
            echo ""
            echo "请确认下载的是对应平台的 SAP NWRFC SDK："
            echo "  macOS   → darwin  → *.dylib"
            echo "  Linux   → linux   → *.so"
            echo "  Windows → windows → *.dll"
            echo "当前平台：$OS-$ARCH（期望 *.$LIB_EXT）"
            exit 1
        fi

        # 验证
        LIB_COUNT=$(find "$SDK_SUBDIR" -maxdepth 1 \( -name "*.dll" -o -name "*.so" -o -name "*.dylib" \) 2>/dev/null | wc -l)
        if [ "$LIB_COUNT" -gt 0 ]; then
            ok "SAP SDK 解压完成: $SDK_SUBDIR ($LIB_COUNT 个库文件)"
            SDK_OK=1
        else
            err "解压后仍未发现库文件，请检查 zip 是否完整"
            exit 1
        fi
    fi
fi

# 3d. 全部失败：清晰指引
if [ "$SDK_OK" = "0" ]; then
    echo ""
    err "未找到 SAP NWRFC SDK 库文件"
    echo ""
    echo "三种方式任选其一："
    echo ""
    echo "  方式 A（推荐）：把 SAP 下载的 zip 放到 nwrfcsdk/lib/<任意>/ 目录下，"
    echo "                 本脚本会自动解压到正确路径。"
    echo ""
    echo "  方式 B：手动把库文件复制到 $SDK_SUBDIR/"
    echo "                 详见 nwrfcsdk/README.md §3。"
    echo ""
    echo "  方式 C：通过环境变量指向已安装路径："
    echo "                 export SAP_SDK_DIR=/path/to/nwrfcsdk"
    echo ""
    echo "获取 SDK：https://launchpad.support.sap.com （需 SAP 账号）"
    exit 1
fi
echo ""

# 3e. macOS：自动修复 SDK dylib 的 install name + 重签
#     SAP 官方 SDK 的 dylib install name 是 @loader_path/xxx（写死），
#     导致 rpath 机制无效、运行时找不到库（dyld: Library not loaded）。
#     改成 @rpath/xxx 让链接器用二进制嵌入的 rpath 自动定位。
#     改 install name 会破坏原有代码签名，需重新 ad-hoc 签名，否则被 SIGKILL。
#     仅 macOS 需要；Linux 用 rpath/LD_LIBRARY_PATH，Windows 用默认 DLL 搜索。
if [ "$OS" = "darwin" ] && [ -d "$SDK_SUBDIR" ]; then
    for dylib in "$SDK_SUBDIR"/*.dylib; do
        [ -e "$dylib" ] || continue
        # 读当前 install name（otool -D 第二行）
        CUR_ID=$(otool -D "$dylib" 2>/dev/null | tail -1)
        BASE=$(basename "$dylib")
        if [[ "$CUR_ID" != "@rpath/$BASE" ]]; then
            echo "macOS：修复 $BASE 的 install name（@loader_path → @rpath）..."
            install_name_tool -id "@rpath/$BASE" "$dylib" 2>/dev/null
            # 重签（ad-hoc），否则签名失效的库加载时被系统 SIGKILL
            codesign --force --sign - "$dylib" 2>/dev/null
            ok "$BASE install name 已修复并重签"
        fi
    done
fi

# 4. .env 配置文件
if [ ! -f ".env" ]; then
    if [ -f ".env.example" ]; then
        warn "未找到 .env，已自动从 .env.example 复制（请编辑后再运行）"
        cp .env.example .env
        echo ""
        echo "请编辑 .env 填入 SAP 连接参数（SAP_ASHOST/SAP_USER/SAP_PASSWD 等），然后重新运行 ./start.sh"
        exit 0
    else
        err "未找到 .env 且无 .env.example 模板"
        exit 1
    fi
else
    ok ".env 配置文件存在"
    if grep -q "your-password-here" .env 2>/dev/null; then
        warn ".env 中仍含占位文本 'your-password-here'，请确认已填入真实 SAP 凭据"
    fi
fi
echo ""

# 5. 端口检查（默认 3000，可通过 SAP_LISTEN_ADDR 覆盖）
LISTEN_ADDR="${SAP_LISTEN_ADDR:-127.0.0.1:3000}"
LISTEN_PORT="${LISTEN_ADDR##*:}"
if command -v ss >/dev/null 2>&1 && ss -tln 2>/dev/null | grep -q ":$LISTEN_PORT\b"; then
    err "端口 $LISTEN_PORT 已被占用"
    echo ""
    echo "处理方式："
    echo "  - 设置 SAP_LISTEN_ADDR 改用其它端口，如："
    echo "      export SAP_LISTEN_ADDR=127.0.0.1:3001"
    echo "  - 或停止占用进程："
    if command -v lsof >/dev/null 2>&1; then
        lsof -i :"$LISTEN_PORT" 2>/dev/null | tail -n +2 || true
    elif command -v fuser >/dev/null 2>&1; then
        fuser "$LISTEN_PORT/tcp" 2>&1 || true
    fi
    exit 1
elif command -v lsof >/dev/null 2>&1 && lsof -iTCP:"$LISTEN_PORT" -sTCP:LISTEN >/dev/null 2>&1; then
    err "端口 $LISTEN_PORT 已被占用（lsof 检测）"
    exit 1
elif [ "$(uname -s)" = "Linux" ] && command -v netstat >/dev/null 2>&1 && netstat -tln 2>/dev/null | grep -q ":$LISTEN_PORT\b"; then
    err "端口 $LISTEN_PORT 已被占用（netstat 检测）"
    exit 1
else
    ok "端口 $LISTEN_PORT 可用"
fi
echo ""

echo "=== 检查通过，启动服务 ==="
echo ""

# SAP SDK 的 ICU 库通过 dlopen 运行时加载（非 Mach-O 链接依赖），
# rpath 管不到 dlopen，必须设环境变量指向 SDK 目录。
# 用绝对路径，避免工作目录差异。
SDK_LIB_ABS="$PWD/$SDK_SUBDIR"
if [ "$OS" = "darwin" ]; then
    export DYLD_LIBRARY_PATH="$SDK_LIB_ABS:${DYLD_LIBRARY_PATH:-}"
elif [ "$OS" = "linux" ]; then
    export LD_LIBRARY_PATH="$SDK_LIB_ABS:${LD_LIBRARY_PATH:-}"
fi

# 守护循环：`POST /api/admin/restart` 会 exit(0) 让进程退出，需要有人把它拉起来，
# 否则一次「重启」就变成永久停机。循环让同一份脚本兼当 supervisor。
# SAP_GUARD=off 退回单次 exec（退出即结束）——交给 launchd/systemd 兜底时用。
# 先构建一次，守护循环直接 exec 二进制——生产重启路径不再要求 cargo 在 PATH、
# 不付 rebuild 检查延迟，进程树也少穿一层 cargo（SIGTERM 语义更直接）
#（2026-11-05 审计 P2）
cargo build --release || { err "cargo build --release 失败，无法启动"; exit 1; }
BIN="$PWD/target/release/sap_for_agents"

if [ "${SAP_GUARD:-on}" = "off" ]; then
    cleanup_lock   # exec 不触发 EXIT trap，锁要在这里放掉，否则下次启动被自己的残留锁拒绝
    exec "$BIN"
fi

# 循环内要读子进程退出码，`set -e` 会让脚本在非 0 退出时直接中止
set +e

STOP=0
RESTARTS=0
CONSECUTIVE_FAILS=0
MAX_CONSECUTIVE_FAILS="${SAP_MAX_RESTARTS:-5}"
BACKOFF=1
MAX_BACKOFF="${SAP_MAX_BACKOFF:-30}"
# 存活满这个秒数再退出，就不算「连续崩溃」（崩溃循环的特征是刚起来就死）——
# 否则跑几天崩一次的服务，攒满 MAX_CONSECUTIVE_FAILS 次就被永久停机。
STABLE_RUN_SECS=60
trap 'STOP=1' INT TERM

# 端口占用检测（循环内复用；只认 LISTEN 状态，不误伤 TIME_WAIT 客户端）
port_is_listening() {
    if command -v lsof >/dev/null 2>&1; then
        lsof -nP -iTCP:"$LISTEN_PORT" -sTCP:LISTEN >/dev/null 2>&1
    elif command -v ss >/dev/null 2>&1; then
        ss -tln 2>/dev/null | grep -q ":$LISTEN_PORT\b"
    elif [ "$(uname -s)" = "Linux" ] && command -v netstat >/dev/null 2>&1; then
        netstat -tln 2>/dev/null | grep -q ":$LISTEN_PORT\b"
    else
        return 1
    fi
}

while [ "$STOP" -eq 0 ]; do
    # 重启前先确认端口空闲：若被别的实例占着，再启动只会 AddrInUse 死循环
    if port_is_listening; then
        err "端口 $LISTEN_PORT 已被占用，放弃重启（避免 AddrInUse 死循环）"
        echo "  占用进程："
        lsof -nP -iTCP:"$LISTEN_PORT" -sTCP:LISTEN 2>/dev/null || true
        echo "  处理：停掉占用进程，或 export SAP_LISTEN_ADDR=127.0.0.1:3001 换端口"
        break
    fi

    # 端口探测期间收到过停止信号的话，不再拉起服务：bash 要等前台命令（lsof/ss）结束
    # 才执行 trap，所以 Ctrl+C 只打断了探测，STOP 是在那之后才置位的——启动前再查一次
    if [ "$STOP" -ne 0 ]; then
        echo ""
        ok "服务已停止（本次运行周期内自动重启 ${RESTARTS} 次）"
        break
    fi

    START_TS=$(date +%s)
    "$BIN"
    RC=$?
    RUN_SECS=$(( $(date +%s) - START_TS ))

    # 收到 Ctrl+C / SIGTERM：不再重启（信号已透传给上面的服务，它会先排空再退）
    if [ "$STOP" -ne 0 ]; then
        echo ""
        ok "服务已停止（本次运行周期内自动重启 $RESTARTS 次）"
        break
    fi

    # 正常退出（如 /api/admin/restart 的 exit(0)）：重置失败计数，短延迟重启
    if [ "$RC" -eq 0 ]; then
        CONSECUTIVE_FAILS=0
        BACKOFF=1
        RESTARTS=$((RESTARTS + 1))
        echo ""
        ok "进程正常退出，1 秒后重启（累计第 $RESTARTS 次）"
        sleep 1
        continue
    fi

    # 异常退出：连续崩溃达上限就停手，不再无限重启。
    # 这次是跑够了 STABLE_RUN_SECS 才倒下的：不算崩溃循环，连续计数从头来。
    if [ "$RUN_SECS" -ge "$STABLE_RUN_SECS" ]; then
        CONSECUTIVE_FAILS=0
        BACKOFF=1
    fi
    CONSECUTIVE_FAILS=$((CONSECUTIVE_FAILS + 1))
    if [ "$CONSECUTIVE_FAILS" -ge "$MAX_CONSECUTIVE_FAILS" ]; then
        err "进程连续崩溃 ${CONSECUTIVE_FAILS} 次（最后 exit=${RC}），达到上限 ${MAX_CONSECUTIVE_FAILS}，停止自动重启"
        echo "  排查：tail -n 100 /private/tmp/gw-*.log"
        echo "  确认修复后重新执行 ./start.sh；如需放宽：export SAP_MAX_RESTARTS=20"
        break
    fi

    RESTARTS=$((RESTARTS + 1))
    echo ""
    warn "进程异常退出（code ${RC}），${BACKOFF}s 后重启（累计第 ${RESTARTS} 次，连续失败 ${CONSECUTIVE_FAILS}/${MAX_CONSECUTIVE_FAILS}）—— Ctrl+C 可停止"
    sleep "$BACKOFF"
    BACKOFF=$((BACKOFF * 2))
    [ "$BACKOFF" -gt "$MAX_BACKOFF" ] && BACKOFF=$MAX_BACKOFF
done
