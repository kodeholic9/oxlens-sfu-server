// author: kodeholic (powered by Claude)
//! Startup helpers — .env 로드, 환경변수 파싱, 기본 방 생성
//!
//! lib.rs에서 분리. 서버 기동 시 1회성 초기화 로직.

use tracing::info;

use crate::config;
use crate::state::AppState;

/// .env 파일 탐색 및 로드
///
/// 우선순위:
///   1. `--env /path/to/.env` CLI 인자 → 해당 경로 (없으면 에러 출력 후 무시)
///   2. CWD/.env
///   3. 실행파일 디렉토리/.env
///   4. 모두 없으면 환경변수/기본값으로 동작
pub(crate) fn load_env_file() {
    let args: Vec<String> = std::env::args().collect();

    // --env /path 인자 처리
    if let Some(pos) = args.iter().position(|a| a == "--env") {
        if let Some(path) = args.get(pos + 1) {
            match dotenvy::from_path(std::path::Path::new(path)) {
                Ok(_) => {
                    eprintln!("[env] loaded: {}", path);
                    return;
                }
                Err(e) => {
                    eprintln!("[env] WARN: --env {} failed: {}", path, e);
                    // fallback으로 계속
                }
            }
        } else {
            eprintln!("[env] WARN: --env requires a path argument");
        }
    }

    // CWD/.env
    if dotenvy::dotenv().is_ok() {
        eprintln!("[env] loaded: .env (CWD)");
        return;
    }

    // 실행파일 디렉토리/.env
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let env_path = dir.join(".env");
            if env_path.is_file() {
                match dotenvy::from_path(&env_path) {
                    Ok(_) => {
                        eprintln!("[env] loaded: {}", env_path.display());
                        return;
                    }
                    Err(e) => {
                        eprintln!("[env] WARN: {} failed: {}", env_path.display(), e);
                    }
                }
            }
        }
    }

    eprintln!("[env] no .env file found, using environment variables / defaults");
}

/// 환경변수에서 값 로드, 실패 시 기본값 반환
pub(crate) fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// 라우팅 테이블 기반 로컬 IP 감지 (PUBLIC_IP 미설정 시 fallback)
pub(crate) fn detect_local_ip() -> String {
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("8.8.8.8:80")?;
            s.local_addr()
        })
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".to_string())
}

/// 서버 기동 시 기본 방 생성 (테스트/개발용)
pub(crate) fn create_default_rooms(state: &AppState) {
    // (name, capacity, mode, simulcast)
    let defaults: [(&str, usize, config::RoomMode, bool); 3] = [
        ("무전 대화방", 10, config::RoomMode::Ptt, false),
        ("회의실-2", 10, config::RoomMode::Conference, false),
        ("대회의실", 20, config::RoomMode::Conference, true),
    ];

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    for (name, capacity, mode, simulcast) in defaults {
        let room = state.rooms.create(name.to_string(), Some(capacity), mode, now, simulcast);
        info!("default room created: {} (id={}, cap={}, mode={}, simulcast={})", name, room.id, capacity, room.mode, room.simulcast_enabled);
    }
}
