#!/bin/bash
# deploy-oxlens.sh — oxlens-sfu-server 서버 배포/운영 스크립트
# author: kodeholic (powered by Claude)
#
# 사용법:
#   ./deploy-oxlens.sh patch     — 전체 배포 (stop → 백업 → git pull → build → start)
#   ./deploy-oxlens.sh start     — 서버 시작
#   ./deploy-oxlens.sh stop      — 서버 종료
#   ./deploy-oxlens.sh restart   — 서버 재시작
#   ./deploy-oxlens.sh status    — 프로세스 상태 확인
#   ./deploy-oxlens.sh setup     — 최초 1회: Rust 설치 + 빌드 의존성 + 초기 배포
#   ./deploy-oxlens.sh log       — 최근 로그 tail

set -euo pipefail

# --- 설정 변수 ---
APP_NAME="oxlens-sfu-server"
BASE_DIR="$HOME/oxsfu"
SRC_DIR="${BASE_DIR}/src"
BIN_DIR="${BASE_DIR}/bin"
BACKUP_DIR="${BASE_DIR}/backup"
LOG_DIR="${BASE_DIR}/logs"
PID_FILE="${BASE_DIR}/oxsfu.pid"
ENV_FILE="${BASE_DIR}/.env"

GIT_REPO_URL="https://github.com/kodeholic9/oxlens-sfu-server.git"
GIT_BRANCH="main"

# 바이너리명 (Cargo.toml [[bin]] name = "oxsfud")
BIN_NAME="oxsfud"

# --- 컬러 출력 ---
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
CYAN='\033[0;36m'
NC='\033[0m' # No Color

info()  { echo -e "${CYAN}[INFO]${NC} $1"; }
ok()    { echo -e "${GREEN}[OK]${NC} $1"; }
warn()  { echo -e "${YELLOW}[WARN]${NC} $1"; }
err()   { echo -e "${RED}[ERROR]${NC} $1"; }

# --- 유틸 함수 ---

# .env 파일 로드 (있으면)
load_env() {
    if [ -f "$ENV_FILE" ]; then
        set -a
        source "$ENV_FILE"
        set +a
    fi
}

# PID 파일로 프로세스 존재 여부 확인
is_running() {
    if [ -f "$PID_FILE" ]; then
        local pid
        pid=$(cat "$PID_FILE")
        if kill -0 "$pid" 2>/dev/null; then
            return 0 # running
        fi
    fi
    return 1 # not running
}

get_pid() {
    if [ -f "$PID_FILE" ]; then
        cat "$PID_FILE"
    fi
}

# --- 명령 함수 ---

do_setup() {
    echo "========================================"
    echo " ${APP_NAME} 초기 설정 (최초 1회)"
    echo "========================================"

    # 1. 디렉토리 생성
    info "디렉토리 구조 생성..."
    mkdir -p "$BIN_DIR" "$BACKUP_DIR" "$LOG_DIR"
    ok "디렉토리 생성 완료"

    # 2. Rust 툴체인 확인/설치
    if command -v cargo &>/dev/null; then
        ok "Rust 툴체인 이미 설치됨: $(rustc --version)"
    else
        info "Rust 툴체인 설치 중... (rustup)"
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
        source "$HOME/.cargo/env"
        ok "Rust 설치 완료: $(rustc --version)"
    fi

    # 3. 빌드 의존성 (openssl, pkg-config 등)
    info "시스템 빌드 의존성 확인..."
    local DEPS_NEEDED=""
    for pkg in build-essential pkg-config libssl-dev; do
        if ! dpkg -s "$pkg" &>/dev/null; then
            DEPS_NEEDED="$DEPS_NEEDED $pkg"
        fi
    done
    if [ -n "$DEPS_NEEDED" ]; then
        info "패키지 설치:$DEPS_NEEDED"
        sudo apt-get update -qq
        sudo apt-get install -y -qq $DEPS_NEEDED
        ok "빌드 의존성 설치 완료"
    else
        ok "빌드 의존성 모두 설치됨"
    fi

    # 4. .env 파일 템플릿 생성 (없으면)
    if [ ! -f "$ENV_FILE" ]; then
        info ".env 템플릿 생성..."
        cat > "$ENV_FILE" << 'EOF'
# oxlens-sfu-server 환경변수
# 로그 레벨: info(기본), debug(상세), trace(패킷)
RUST_LOG=oxlens_sfu_server=info
EOF
        ok ".env 파일 생성됨: ${ENV_FILE}"
    fi

    # 5. 최초 클론 + 빌드
    do_clone_and_build

    ok "초기 설정 완료! 다음으로 실행하세요:"
    echo "  ./deploy-oxlens.sh start"
}

do_clone_and_build() {
    # cargo 확인
    if ! command -v cargo &>/dev/null; then
        if [ -f "$HOME/.cargo/env" ]; then
            source "$HOME/.cargo/env"
        fi
        if ! command -v cargo &>/dev/null; then
            err "cargo를 찾을 수 없습니다. 먼저 ./deploy-oxlens.sh setup 실행하세요."
            return 1
        fi
    fi

    # Git clone or pull
    if [ -d "${SRC_DIR}/.git" ]; then
        info "기존 소스 업데이트 (git pull)..."
        cd "$SRC_DIR"
        git fetch origin
        git checkout "$GIT_BRANCH"
        git reset --hard "origin/${GIT_BRANCH}"
        ok "소스 업데이트 완료"
    else
        info "소스 클론 중: ${GIT_REPO_URL} (${GIT_BRANCH})..."
        rm -rf "$SRC_DIR"
        git clone --single-branch --branch "$GIT_BRANCH" "$GIT_REPO_URL" "$SRC_DIR"
        ok "클론 완료"
    fi

    # 빌드
    cd "$SRC_DIR"
    info "cargo build --release 시작... (시간이 걸릴 수 있습니다)"
    cargo build --release 2>&1 | tail -5
    ok "빌드 완료"

    # 바이너리 복사
    info "바이너리 복사 → ${BIN_DIR}/"
    if [ -f "target/release/${BIN_NAME}" ]; then
        cp "target/release/${BIN_NAME}" "${BIN_DIR}/${BIN_NAME}"
        chmod +x "${BIN_DIR}/${BIN_NAME}"
        ok "  ${BIN_NAME} $(du -h "${BIN_DIR}/${BIN_NAME}" | cut -f1)"
    else
        err "  ${BIN_NAME} 바이너리를 찾을 수 없습니다"
        return 1
    fi
}

do_backup() {
    info "현재 바이너리 백업 중..."
    if [ -f "${BIN_DIR}/${BIN_NAME}" ]; then
        local TIMESTAMP
        TIMESTAMP=$(date +"%Y%m%d_%H%M%S")
        local BACKUP_FILE="${BACKUP_DIR}/oxsfu_${TIMESTAMP}.tar.gz"
        tar -czf "$BACKUP_FILE" -C "$BIN_DIR" .
        ok "백업 완료: ${BACKUP_FILE}"

        # 오래된 백업 정리 (최근 5개만 유지)
        local BACKUP_COUNT
        BACKUP_COUNT=$(ls -1 "${BACKUP_DIR}"/oxsfu_*.tar.gz 2>/dev/null | wc -l)
        if [ "$BACKUP_COUNT" -gt 5 ]; then
            ls -1t "${BACKUP_DIR}"/oxsfu_*.tar.gz | tail -n +6 | xargs rm -f
            info "오래된 백업 정리 (최근 5개 유지)"
        fi
    else
        warn "백업할 바이너리가 없습니다 (최초 배포)"
    fi
}

do_start() {
    if is_running; then
        warn "이미 실행 중입니다 (PID: $(get_pid))"
        return 0
    fi

    if [ ! -f "${BIN_DIR}/${BIN_NAME}" ]; then
        err "${BIN_NAME} 바이너리가 없습니다. 먼저 patch 또는 setup을 실행하세요."
        return 1
    fi

    load_env

    local LOG_FILE="${LOG_DIR}/${BIN_NAME}_$(date +"%Y%m%d").log"

    info "${BIN_NAME} 시작 중..."
    nohup "${BIN_DIR}/${BIN_NAME}" >> "$LOG_FILE" 2>&1 &
    local PID=$!
    echo "$PID" > "$PID_FILE"

    # 시작 확인 (1초 대기 후 프로세스 존재 여부)
    sleep 1
    if kill -0 "$PID" 2>/dev/null; then
        ok "${BIN_NAME} 시작 완료 (PID: ${PID})"
        ok "로그: ${LOG_FILE}"
    else
        err "${BIN_NAME} 시작 실패! 로그를 확인하세요:"
        echo "  tail -50 ${LOG_FILE}"
        rm -f "$PID_FILE"
        return 1
    fi
}

do_stop() {
    if ! is_running; then
        warn "실행 중인 프로세스가 없습니다"
        rm -f "$PID_FILE"
        return 0
    fi

    local PID
    PID=$(get_pid)
    info "${BIN_NAME} 종료 중... (PID: ${PID})"

    # SIGTERM → graceful shutdown (Ctrl+C와 동일)
    kill "$PID" 2>/dev/null

    # 최대 10초 대기 (drain 3s + 여유)
    local WAIT=0
    while kill -0 "$PID" 2>/dev/null && [ "$WAIT" -lt 10 ]; do
        sleep 1
        WAIT=$((WAIT + 1))
    done

    if kill -0 "$PID" 2>/dev/null; then
        warn "SIGTERM 타임아웃, SIGKILL 전송..."
        kill -9 "$PID" 2>/dev/null
        sleep 1
    fi

    rm -f "$PID_FILE"
    ok "${BIN_NAME} 종료 완료"
}

do_restart() {
    do_stop
    do_start
}

do_status() {
    echo "========================================"
    echo " ${APP_NAME} 상태"
    echo "========================================"

    if is_running; then
        local PID
        PID=$(get_pid)
        ok "${BIN_NAME} 실행 중 (PID: ${PID})"
        echo ""
        ps -p "$PID" -o pid,ppid,etime,%cpu,%mem,rss,cmd --no-headers 2>/dev/null || true
    else
        warn "${BIN_NAME} 실행 중이 아닙니다"
    fi

    echo ""
    if [ -f "${BIN_DIR}/${BIN_NAME}" ]; then
        info "바이너리:"
        echo "  ${BIN_NAME}  $(du -h "${BIN_DIR}/${BIN_NAME}" | cut -f1)  $(date -r "${BIN_DIR}/${BIN_NAME}" "+%Y-%m-%d %H:%M:%S")"
    else
        warn "바이너리가 설치되지 않았습니다"
    fi

    echo ""
    local LATEST_LOG
    LATEST_LOG=$(ls -1t "${LOG_DIR}"/${BIN_NAME}_*.log 2>/dev/null | head -1)
    if [ -n "$LATEST_LOG" ]; then
        info "최근 로그 (${LATEST_LOG}):"
        tail -5 "$LATEST_LOG"
    fi
}

do_patch() {
    echo "========================================"
    echo " ${APP_NAME} 패치 배포"
    echo "========================================"

    # 1. 프로세스 중지 (바이너리 교체를 위해 먼저 stop)
    if is_running; then
        info "패치를 위해 서버 중지..."
        do_stop
    fi

    # 2. 백업
    do_backup

    # 3. Git pull + build + 바이너리 복사
    if ! do_clone_and_build; then
        err "빌드 실패! 기존 바이너리로 복구 시작..."
        # 가장 최근 백업에서 복구
        local LATEST_BACKUP
        LATEST_BACKUP=$(ls -1t "${BACKUP_DIR}"/oxsfu_*.tar.gz 2>/dev/null | head -1)
        if [ -n "$LATEST_BACKUP" ]; then
            tar -xzf "$LATEST_BACKUP" -C "$BIN_DIR"
            ok "백업에서 복구 완료: ${LATEST_BACKUP}"
        fi
        do_start
        return 1
    fi

    # 4. 시작
    do_start

    echo "========================================"
    ok "패치 배포 완료!"
    echo "========================================"
}

do_log() {
    local LATEST_LOG
    LATEST_LOG=$(ls -1t "${LOG_DIR}"/${BIN_NAME}_*.log 2>/dev/null | head -1)
    if [ -n "$LATEST_LOG" ]; then
        info "로그 파일: ${LATEST_LOG}"
        tail -f "$LATEST_LOG"
    else
        warn "로그 파일이 없습니다"
    fi
}

# --- 명령 디스패치 ---
COMMAND=${1:-""}

case "$COMMAND" in
    setup)   do_setup   ;;
    start)   do_start   ;;
    stop)    do_stop    ;;
    restart) do_restart ;;
    status)  do_status  ;;
    patch)   do_patch   ;;
    log)     do_log     ;;
    *)
        echo "사용법: $0 [setup|start|stop|restart|status|patch|log]"
        echo ""
        echo "  setup    최초 1회: Rust 설치 + 의존성 + 빌드 + 환경 구성"
        echo "  start    서버 시작 (nohup, WS:1974 UDP:19740)"
        echo "  stop     서버 종료 (graceful, SIGTERM → 10s → SIGKILL)"
        echo "  restart  서버 재시작"
        echo "  status   프로세스 상태 + 바이너리 정보"
        echo "  patch    전체 배포 (stop → 백업 → git pull → build → start)"
        echo "  log      최근 로그 tail -f"
        exit 1
        ;;
esac