# OxLens SFU Server — 운영 가이드

## 1. 포트 구성

| 포트 | 프로토콜 | 용도 | .env 키 |
|------|----------|------|---------|
| 1974 | TCP (WebSocket) | 시그널링 (Axum HTTP/WS) | `WS_PORT` |
| 19740 | UDP | 미디어 (STUN/DTLS/SRTP) | `UDP_PORT` |

> 기본값: `src/config.rs` / `.env`로 오버라이드 가능

---

## 2. 환경 설정 (.env)

### 2.1 .env 파일 탐색 순서

서버 기동 시 아래 순서로 `.env` 파일을 탐색합니다.
먼저 발견된 파일 하나만 로드하고, 모두 없으면 환경변수/기본값으로 동작합니다.

| 순서 | 조건 | 경로 |
|------|------|------|
| 1 | `--env` 인자 지정 | `oxsfud --env /opt/oxsfu/.env` |
| 2 | CWD (현재 작업 디렉토리) | `./.env` |
| 3 | 실행파일 디렉토리 | `/opt/oxsfu/.env` (바이너리와 같은 폴더) |
| 4 | 없음 | 환경변수 또는 `config.rs` 기본값 |

기동 시 어떤 경로를 로드했는지 stderr로 출력됩니다:
```
[env] loaded: /opt/oxsfu/.env
```

### 2.2 설정 항목

| 키 | 기본값 | 설명 |
|----|--------|------|
| `PUBLIC_IP` | 자동 감지 (내부망 IP) | ICE candidate에 노출할 IP. **LTE/외부 접속 시 공인 IP 필수** |
| `WS_PORT` | 1974 | WebSocket 시그널링 포트 |
| `UDP_PORT` | 19740 | UDP 미디어 포트 |
| `LOG_DIR` | (미설정 = 콘솔) | 로그 파일 디렉토리. 디렉토리가 존재해야 활성화 |
| `LOG_LEVEL` | info | 로그 레벨 (`RUST_LOG` 미설정 시 fallback) |
| `RUST_LOG` | — | 설정 시 `LOG_LEVEL`보다 우선. 모듈별 세밀 제어 가능 |

### 2.3 .env 예시

```env
# ── 네트워크 ──
PUBLIC_IP=121.160.xxx.xxx
WS_PORT=1974
UDP_PORT=19740

# ── 로그 ──
LOG_DIR=/var/log/oxsfu
LOG_LEVEL=info
```

### 2.4 사용 예시

```bash
# 기본: CWD의 .env 자동 탐색
./oxsfud

# 명시적 경로
./oxsfud --env /opt/oxsfu/production.env

# .env 없이 환경변수만 (Docker, systemd 등)
PUBLIC_IP=1.2.3.4 LOG_LEVEL=debug ./oxsfud

# 개발: .env 없이 기본값 (내부망 IP 자동 감지, 콘솔 로그)
cargo run
```

---

## 3. 로그 설정

### 3.1 출력 대상

| `LOG_DIR` 설정 | 동작 |
|----------------|------|
| 미설정 또는 빈 문자열 | **콘솔(stdout)** 출력 |
| 경로 지정 + 디렉토리 존재 | **일별 로테이션 파일** (`oxsfud.log.YYYY-MM-DD`) |
| 경로 지정 + 디렉토리 없음 | **콘솔 fallback** (디렉토리 자동 생성하지 않음) |

파일 출력 시 ANSI 색상 코드는 자동 제거됩니다.

### 3.2 로그 레벨

우선순위: `RUST_LOG` > `LOG_LEVEL` > 기본값(`info`)

```
error  에러 (SRTP 키 실패, 시스템 에러)
warn   경고 (DTLS 타임아웃, 좀비 reaper)
info   운영 필수 (접속/해제, STUN latch, DTLS ready, 방 입장/퇴장)
debug  진단용 (패킷 단위 로그 앞 50건, RTCP 상세)
trace  패킷 레벨 (요약 로그, 매우 많은 출력)
```

### 3.3 oxlens_sfu_server 로그만 남기기

`RUST_LOG`를 사용하면 의존성 크레이트(dtls, webrtc-srtp, tokio 등)의 로그를 제외하고
**oxlens_sfu_server 모듈 로그만** 남길 수 있습니다.

```env
# .env — oxlens_sfu_server만 info 레벨로 (의존성 로그 전부 차단)
RUST_LOG=oxlens_sfu_server=info
```

```env
# .env — oxlens_sfu_server만 debug, 나머지 전부 차단
RUST_LOG=oxlens_sfu_server=debug
```

`RUST_LOG`를 설정하지 않으면 `LOG_LEVEL` 값이 자동으로 `oxlens_sfu_server={LOG_LEVEL}`로 변환되어
기본적으로 의존성 로그는 차단됩니다.

즉, 대부분의 경우 `.env`에 `LOG_LEVEL=info`만 써도 의도한 대로 동작합니다.

### 3.4 모듈별 세밀 제어

```env
# 시그널링은 debug, 미디어는 info, 나머지 의존성은 차단
RUST_LOG=oxlens_sfu_server::signaling=debug,oxlens_sfu_server::transport=info

# UDP hot path만 trace (패킷 단위 추적, 로그 폭발 주의)
RUST_LOG=oxlens_sfu_server::transport::udp=trace,oxlens_sfu_server=info

# 의존성 크레이트 포함 전체 debug (DTLS 핸드셰이크 디버깅 등)
RUST_LOG=debug
```

### 3.5 로그 파일 관리

```bash
# 로그 디렉토리 생성 (최초 1회)
sudo mkdir -p /var/log/oxsfu
sudo chown oxsfu:oxsfu /var/log/oxsfu

# 로그 파일 확인
ls -la /var/log/oxsfu/
# oxsfud.log.2026-03-05
# oxsfud.log.2026-03-04
# ...

# 실시간 tail
tail -f /var/log/oxsfu/oxsfud.log.*

# 오래된 로그 정리 (30일 이전)
find /var/log/oxsfu -name "oxsfud.log.*" -mtime +30 -delete
```

---

## 4. 개발 환경 실행

### 4.1 기본 실행

```bash
cargo run
```

`.env` 없이 실행하면 내부망 IP 자동 감지, 콘솔 로그, 기본 포트로 동작합니다.

### 4.2 Windows (PowerShell)

```powershell
$env:RUST_LOG="oxlens_sfu_server=debug"
cargo run

# 또는 .env 파일 생성 후
cargo run
```

### 4.3 Windows (cmd)

```cmd
set RUST_LOG=oxlens_sfu_server=debug
cargo run
```

### 4.4 정상 기동 로그 예시

```
[env] loaded: .env (CWD)
2026-03-05 12:30:01.234  INFO config: PUBLIC_IP=121.160.xxx.xxx WS_PORT=1974 UDP_PORT=19740 LOG_DIR=/var/log/oxsfu
2026-03-05 12:30:01.235  INFO DTLS fingerprint: sha-256 AB:CD:EF:...
2026-03-05 12:30:01.236  INFO UDP listening on port 19740
2026-03-05 12:30:01.237  INFO oxlens-sfu-server v0.3.4 listening on 0.0.0.0:1974
```

### 4.5 클라이언트 접속 시 로그 예시

```
2026-03-05 12:30:10.100  INFO ROOM_JOIN user=U0042 room=abc-def
2026-03-05 12:30:10.300  INFO [DBG:STUN] latch user=U0042 pc=pub ufrag=Kx7m addr=192.168.0.10:54321
2026-03-05 12:30:10.800  INFO [DBG:DTLS] SRTP ready user=U0042 pc=pub addr=192.168.0.10:54321
```

---

## 5. 운영 서버 배포

### 5.1 빌드

```bash
# 라즈베리파이에서 직접 빌드
cargo build --release
```

산출물: `target/release/oxsfud` (단일 바이너리, 의존성 없음)

### 5.2 배포 절차

```bash
# 1. .env 준비
cp .env.example .env
vi .env   # PUBLIC_IP, LOG_DIR 설정

# 2. 로그 디렉토리 생성
mkdir -p /var/log/oxsfu

# 3. 빌드 & 실행
cargo build --release
./target/release/oxsfud
# 또는
./target/release/oxsfud --env /opt/oxsfu/.env
```

### 5.3 deploy 스크립트

기존 `deploy-oxlens.sh`의 순서: `stop → backup → build → start`

```bash
# 바이너리명: oxsfud
# .env는 실행파일과 같은 디렉토리에 배치하면 자동 탐색
```

### 5.4 systemd 서비스

```ini
# /etc/systemd/system/oxsfud.service
[Unit]
Description=OxLens SFU Server
After=network.target

[Service]
Type=simple
User=oxsfu
Group=oxsfu
WorkingDirectory=/opt/oxsfu
ExecStart=/opt/oxsfu/oxsfud --env /opt/oxsfu/.env
Restart=always
RestartSec=3

# .env에서 로그/포트 관리하므로 Environment 불필요
# 필요시 오버라이드:
# Environment=RUST_LOG=oxlens_sfu_server=debug

StandardOutput=journal
StandardError=journal
LimitNOFILE=65535

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable oxsfud
sudo systemctl start oxsfud
sudo systemctl status oxsfud

# 로그 확인 (LOG_DIR 미설정 시 journald 사용)
sudo journalctl -u oxsfud -f
```

---

## 6. 방화벽 설정

```bash
# Ubuntu (ufw)
sudo ufw allow 1974/tcp    # WebSocket
sudo ufw allow 19740/udp   # Media

# CentOS/RHEL (firewalld)
sudo firewall-cmd --permanent --add-port=1974/tcp
sudo firewall-cmd --permanent --add-port=19740/udp
sudo firewall-cmd --reload
```

> 포트를 `.env`에서 변경한 경우 방화벽도 맞춰야 합니다.

---

## 7. Nginx 리버스 프록시 (WS만)

WebSocket은 Nginx 뒤에 둘 수 있지만, UDP 미디어는 직접 노출해야 합니다.

```nginx
upstream oxsfu_ws {
    server 127.0.0.1:1974;
}

server {
    listen 443 ssl;
    server_name sfu.oxlens.com;

    ssl_certificate     /etc/letsencrypt/live/sfu.oxlens.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/sfu.oxlens.com/privkey.pem;

    location /ws {
        proxy_pass http://oxsfu_ws;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_read_timeout 86400s;
    }

    location / {
        root /opt/insight-lens;
        index index.html;
        try_files $uri $uri/ =404;
    }
}
```

---

## 8. 모니터링

### 8.1 프로세스 확인

```bash
ps aux | grep oxsfud
ss -tlnp | grep 1974     # TCP (WS)
ss -ulnp | grep 19740    # UDP (Media)
```

### 8.2 연결 수 확인

```bash
ss -tn | grep :1974 | grep ESTAB | wc -l   # WebSocket
ss -un | grep :19740 | wc -l               # UDP
```

### 8.3 로그 기반 모니터링

```bash
# 파일 로그 사용 시
tail -f /var/log/oxsfu/oxsfud.log.* | grep -E "ERROR|WARN"

# journald 사용 시
journalctl -u oxsfud -f | grep -E "ERROR|WARN"

# 방 입장/퇴장 추적
grep "ROOM_JOIN\|participant removed" /var/log/oxsfu/oxsfud.log.*
```

---

## 9. 트러블슈팅

### 9.1 LTE/외부에서 미디어 안 됨

`.env`의 `PUBLIC_IP`가 공인 IP인지 확인:

```bash
# 서버에서 공인 IP 확인
curl -s ifconfig.me

# .env에 반영
PUBLIC_IP=121.160.xxx.xxx
```

`PUBLIC_IP` 미설정 시 라우팅 테이블 기반 내부 IP가 ICE candidate에 들어가므로
같은 LAN에서만 통신됩니다.

### 9.2 WS 연결 안 됨

```bash
ss -tlnp | grep 1974
curl -v http://127.0.0.1:1974/ws
```

### 9.3 DTLS 핸드셰이크 타임아웃

```
WARN DTLS handshake timeout (10s) user=U0042
```

원인: UDP 방화벽 미오픈, 일부 방화벽의 DTLS 필터링, NAT UDP 타임아웃

### 9.4 의존성 크레이트 디버깅

DTLS 핸드셰이크나 SRTP 내부 오류를 봐야 할 때만 의존성 로그를 킵니다:

```bash
RUST_LOG=debug ./oxsfud
# 또는
RUST_LOG=dtls=debug,webrtc_srtp=debug,oxlens_sfu_server=info ./oxsfud
```

운영 환경에서는 반드시 원복: `RUST_LOG=oxlens_sfu_server=info`

---

## 10. 설정 참조

### 10.1 .env 설정 (런타임)

| 키 | 기본값 | 설명 |
|----|--------|------|
| `PUBLIC_IP` | 자동 감지 | ICE candidate IP |
| `WS_PORT` | 1974 | WebSocket 포트 |
| `UDP_PORT` | 19740 | UDP 포트 |
| `LOG_DIR` | (콘솔) | 로그 디렉토리 |
| `LOG_LEVEL` | info | 로그 레벨 |
| `RUST_LOG` | — | 모듈별 로그 (우선) |

### 10.2 config.rs 상수 (컴파일타임)

| 상수 | 값 | 설명 |
|------|-----|------|
| `HEARTBEAT_INTERVAL_MS` | 30,000 | 하트비트 간격 |
| `HEARTBEAT_TIMEOUT_MS` | 90,000 | 하트비트 타임아웃 |
| `ZOMBIE_TIMEOUT_MS` | 120,000 | 좀비 판정 시간 |
| `ROOM_MAX_CAPACITY` | 1,000 | 방 최대 인원 |
| `RTP_CACHE_SIZE` | 512 | RTX 캐시 슬롯 수 |
| `RTX_BUDGET_PER_3S` | 200 | subscriber별 3초당 RTX 상한 |
| `REMB_BITRATE_BPS` | 500,000 | 서버 REMB 힌트 (bps) |
| `REMB_INTERVAL_MS` | 1,000 | REMB 전송 주기 |
| `FLOOR_MAX_BURST_MS` | 30,000 | PTT 최대 발화 시간 |
| `FLOOR_PING_TIMEOUT_MS` | 5,000 | 발화자 ping 타임아웃 |
