# Light LiveChat — 운영 가이드

## 1. 포트 구성

| 포트 | 프로토콜 | 용도 |
|------|----------|------|
| 1974 | TCP (WebSocket) | 시그널링 (Axum HTTP/WS) |
| 19740 | UDP | 미디어 (STUN/DTLS/SRTP) |

> 소스: `src/config.rs`

---

## 2. 개발 환경 실행

### 2.1 기본 실행

```bash
cargo run
```

기본 로그 레벨은 `light_livechat=debug` (lib.rs EnvFilter 기본값).

### 2.2 로그 레벨 조정

`RUST_LOG` 환경변수로 제어. tracing 레벨: `error` < `warn` < `info` < `debug` < `trace`

```bash
# 전체 debug (기본과 동일)
RUST_LOG=debug cargo run

# info만 (STUN/SRTP trace 숨김)
RUST_LOG=info cargo run

# 모듈별 세밀 제어: 시그널링은 debug, 미디어는 info
RUST_LOG=light_livechat::signaling=debug,light_livechat::transport=info cargo run

# SDP 파싱만 trace (개발 중 SDP 디버깅)
RUST_LOG=light_livechat::transport::sdp=trace,light_livechat=info cargo run

# SRTP hot path trace (패킷 단위 추적, 매우 많은 로그)
RUST_LOG=light_livechat::transport::udp=trace cargo run

# 전체 trace (모든 패킷, 주의: 로그 폭발)
RUST_LOG=trace cargo run
```

### 2.3 Windows (PowerShell)

```powershell
# PowerShell에서는 환경변수를 먼저 설정
$env:RUST_LOG="debug"
cargo run

# 한 줄로
$env:RUST_LOG="light_livechat::signaling=debug,light_livechat::transport::udp=info"; cargo run
```

### 2.4 Windows (cmd)

```cmd
set RUST_LOG=debug
cargo run
```

### 2.5 릴리스 빌드 테스트

```bash
cargo build --release
RUST_LOG=info ./target/release/light-livechat
```

### 2.6 정상 기동 로그 예시

```
2026-03-04T15:30:01.234Z  INFO light_livechat: DTLS fingerprint: sha-256 AB:CD:EF:...
2026-03-04T15:30:01.235Z  INFO light_livechat::transport::udp: UDP transport bound on 0.0.0.0:19740
2026-03-04T15:30:01.235Z  INFO light_livechat: UDP listening on port 19740
2026-03-04T15:30:01.236Z  INFO light_livechat: light-livechat v0.1.4 listening on 0.0.0.0:1974
```

이 4줄이 나오면 정상 기동 완료:
1. DTLS 인증서 생성 + fingerprint 출력
2. UDP 소켓 바인드 (19740)
3. UDP 리스닝 확인
4. WS 서버 리스닝 (1974)

### 2.7 클라이언트 접속 시 로그 예시

```
DEBUG light_livechat::signaling::handler: WS connected from 127.0.0.1:52341
DEBUG light_livechat::signaling::handler: HELLO sent heartbeat_interval=30000
DEBUG light_livechat::signaling::handler: IDENTIFY user=U0042
 INFO light_livechat::signaling::handler: ROOM_JOIN user=U0042 room=abc-def ufrag=Kx7m sections=2
DEBUG light_livechat::transport::udp: USE-CANDIDATE user=U0042 → starting DTLS
 INFO light_livechat::transport::udp: DTLS+SRTP ready user=U0042 addr=192.168.0.10:54321
```

---

## 3. 운영 서버 배포

### 3.1 빌드

```bash
# 크로스 컴파일 (로컬 Mac/Windows → Linux 서버)
# 먼저 target 추가
rustup target add x86_64-unknown-linux-gnu
cargo build --release --target x86_64-unknown-linux-gnu

# 같은 OS라면 단순히
cargo build --release
```

산출물: `target/release/light-livechat` (단일 바이너리, 의존성 없음)

### 3.2 서버 배포

```bash
# 바이너리 전송
scp target/release/light-livechat user@server:/opt/light-livechat/

# 서버에서 직접 빌드하는 경우
ssh user@server
cd /opt/light-livechat
git pull
cargo build --release
cp target/release/light-livechat ./light-livechat
```

### 3.3 직접 실행

```bash
# foreground (로그 확인용)
RUST_LOG=info /opt/light-livechat/light-livechat

# background
RUST_LOG=info nohup /opt/light-livechat/light-livechat > /var/log/light-livechat.log 2>&1 &

# 로그 파일 분리 (stdout + stderr)
RUST_LOG=info /opt/light-livechat/light-livechat \
  > /var/log/light-livechat.out.log \
  2> /var/log/light-livechat.err.log &
```

### 3.4 systemd 서비스

```ini
# /etc/systemd/system/light-livechat.service
[Unit]
Description=Light LiveChat SFU Server
After=network.target

[Service]
Type=simple
User=livechat
Group=livechat
WorkingDirectory=/opt/light-livechat
ExecStart=/opt/light-livechat/light-livechat
Restart=always
RestartSec=3

# 환경변수
Environment=RUST_LOG=info

# 로그 → journald (별도 파일 불필요)
StandardOutput=journal
StandardError=journal

# UDP 포트 권한 (1024 이상이므로 불필요하지만 명시)
# AmbientCapabilities=CAP_NET_BIND_SERVICE

# 리소스 제한
LimitNOFILE=65535

[Install]
WantedBy=multi-user.target
```

```bash
# 서비스 등록 및 시작
sudo systemctl daemon-reload
sudo systemctl enable light-livechat
sudo systemctl start light-livechat

# 상태 확인
sudo systemctl status light-livechat

# 로그 확인
sudo journalctl -u light-livechat -f              # 실시간 tail
sudo journalctl -u light-livechat --since today    # 오늘 로그
sudo journalctl -u light-livechat -n 100           # 최근 100줄
sudo journalctl -u light-livechat --since "2026-03-04 15:00" --until "2026-03-04 16:00"

# 재시작
sudo systemctl restart light-livechat

# 중지
sudo systemctl stop light-livechat
```

### 3.5 로그 레벨 런타임 변경 (재시작 방식)

```bash
# info → debug 로 변경 후 재시작
sudo systemctl set-environment RUST_LOG=debug
sudo systemctl restart light-livechat

# 원복
sudo systemctl set-environment RUST_LOG=info
sudo systemctl restart light-livechat
```

또는 서비스 파일의 `Environment=` 수정 후:

```bash
sudo systemctl daemon-reload
sudo systemctl restart light-livechat
```

---

## 4. 방화벽 설정

```bash
# Ubuntu (ufw)
sudo ufw allow 1974/tcp    # WebSocket
sudo ufw allow 19740/udp   # Media

# CentOS/RHEL (firewalld)
sudo firewall-cmd --permanent --add-port=1974/tcp
sudo firewall-cmd --permanent --add-port=19740/udp
sudo firewall-cmd --reload

# iptables 직접
sudo iptables -A INPUT -p tcp --dport 1974 -j ACCEPT
sudo iptables -A INPUT -p udp --dport 19740 -j ACCEPT
```

---

## 5. Nginx 리버스 프록시 (WS만)

WebSocket은 Nginx 뒤에 둘 수 있지만, UDP 미디어는 직접 노출해야 합니다.

```nginx
# /etc/nginx/sites-available/light-livechat
upstream livechat_ws {
    server 127.0.0.1:1974;
}

server {
    listen 443 ssl;
    server_name livechat.example.com;

    ssl_certificate     /etc/letsencrypt/live/livechat.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/livechat.example.com/privkey.pem;

    # WebSocket
    location /ws {
        proxy_pass http://livechat_ws;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_read_timeout 86400s;  # WS keep-alive (24h)
    }

    # 클라이언트 정적 파일 (선택)
    location / {
        root /opt/insight-lens;
        index index.html;
        try_files $uri $uri/ =404;
    }
}
```

이 구성에서 클라이언트 접속:
- WS: `wss://livechat.example.com/ws` (Nginx → 1974)
- UDP: `livechat.example.com:19740` (직접, SDP answer의 candidate에 포함)

---

## 6. 모니터링

### 6.1 프로세스 확인

```bash
# 프로세스 존재 확인
ps aux | grep light-livechat
pgrep -f light-livechat

# 포트 리스닝 확인
ss -tlnp | grep 1974     # TCP (WS)
ss -ulnp | grep 19740    # UDP (Media)

# netstat (구 버전)
netstat -tlnp | grep 1974
netstat -ulnp | grep 19740
```

### 6.2 연결 수 확인

```bash
# WebSocket 연결 수
ss -tn | grep :1974 | grep ESTAB | wc -l

# UDP 통신 중인 원격 주소 수 (대략적)
ss -un | grep :19740 | wc -l
```

### 6.3 로그 기반 모니터링

```bash
# 실시간 에러만
journalctl -u light-livechat -f | grep -E "ERROR|WARN"

# DTLS 핸드셰이크 실패 추적
journalctl -u light-livechat -f | grep "DTLS handshake"

# 방 입장/퇴장 추적
journalctl -u light-livechat -f | grep "ROOM_JOIN\|participant removed"

# SDP 파싱 에러
journalctl -u light-livechat -f | grep "SDP parse failed"
```

---

## 7. 트러블슈팅

### 7.1 WS 연결 안 됨

```bash
# 포트 리스닝 확인
ss -tlnp | grep 1974

# 방화벽 확인
sudo ufw status | grep 1974

# 로컬 테스트
curl -v http://127.0.0.1:1974/ws
# → 101 Switching Protocols 나오면 정상 (WebSocket upgrade)
```

### 7.2 미디어 안 들림 (SRTP 실패)

```bash
# UDP 포트 확인
ss -ulnp | grep 19740

# UDP 방화벽
sudo ufw status | grep 19740

# DTLS 핸드셰이크 로그 확인
RUST_LOG=light_livechat::transport::udp=debug journalctl -u light-livechat -f
```

자주 있는 원인:
- UDP 방화벽 미오픈 (TCP만 열고 UDP를 빠뜨리는 경우)
- NAT 뒤에서 UDP 타임아웃 (기본 30초, keepalive 필요)
- 클라이언트 SDP offer에 candidate가 없음 (ICE gathering 실패)

### 7.3 DTLS 핸드셰이크 타임아웃 (10s)

```
WARN DTLS handshake timeout (10s) user=U0042 addr=...
```

원인:
- STUN은 통과했으나 DTLS 패킷이 차단됨 (일부 방화벽이 DTLS를 필터링)
- 서버 인증서 문제 (self-signed이므로 클라이언트가 거부하진 않음)
- 동시 다수 연결 시 CPU 병목

### 7.4 SDP 파싱 실패

```
WARN SDP parse failed user=U0042: no media section in SDP offer
```

원인:
- 클라이언트가 빈 SDP 또는 비정상 offer 전송
- `getUserMedia` 실패 후 빈 offer 생성

---

## 8. 설정 변경 참조 (config.rs)

현재 모든 설정은 `src/config.rs`에 상수로 정의되어 있습니다.
변경 시 재컴파일 필요합니다.

| 상수 | 값 | 설명 |
|------|-----|------|
| `WS_PORT` | 1974 | WebSocket 포트 |
| `UDP_PORT` | 19740 | 미디어 UDP 포트 |
| `HEARTBEAT_INTERVAL_MS` | 30,000 | 하트비트 간격 (30초) |
| `HEARTBEAT_TIMEOUT_MS` | 90,000 | 하트비트 타임아웃 (90초) |
| `ROOM_MAX_CAPACITY` | 20 | 방 최대 인원 |
| `ROOM_DEFAULT_CAPACITY` | 10 | 방 기본 인원 |
| `UDP_RECV_BUF_SIZE` | 2048 | UDP 수신 버퍼 크기 |
