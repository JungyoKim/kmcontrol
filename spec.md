# 노트북 관리 시스템 구현 사양서

## 개요

학생 노트북 다수를 관리자(1명 이상)가 원격으로 관리하는 시스템. 두 가지 제어 경로를 제공한다:
1. **MCP 기반 AI 자동 진단/트러블슈팅** (명령 실행, 상태 조회, 필요시 GUI 자동화)
2. **Sunshine/Moonlight 기반 사람의 수동 원격 제어** (실시간 화면 스트리밍 + 입력 제어)

---

## 시스템 구성 요소

| 컴포넌트 | 역할 | 배포 대상 |
|---|---|---|
| `hub-server` | 세션 조율/브로커, 상태 집계, 계정 프로비저닝 API | 상시 구동 서버(1대) |
| `agent` | 노트북에 상주, hub와 통신, Sunshine 관리, 명령 실행 | 학생 노트북 전체 |
| `admin-client` | 관리자용 Tauri 데스크톱 앱 | 관리자 기기(N대) |
| `web-client` + `streaming-bridge` | (추후 구현, 현재 범위 아님 — 확장 가능하게만 설계) | — |

`agent`는 배포 형태 2종(WTG 내장형 / 일반 설치형)이 있으나 **핵심 로직은 완전히 동일한 하나의 코드베이스**이며, 배포 래퍼(WTG `provision.ps1` vs `deploy/install.ps1` 한 줄 부트스트랩)만 다르다. 원안의 MSI/NSIS 인스톨러는 **미채용** — 아래 "배포 형태별 차이" 참고.

---

## 아키텍처 제약 (반드시 지킬 것)

1. **`hub-server`는 영상/스트리밍 바이트를 절대 프록시하지 않는다.** 세션 인증·조율(누가 어느 노트북을 지금 제어 중인지, 권한 확인)만 담당하고, 실제 영상 데이터는 `admin-client` ↔ `agent` 간 직접 P2P(Tailscale)로 흐른다.
2. `hub-server`는 tsnet(임베디드 Tailscale) 노드로 동작한다 — 가벼운 트래픽만 처리하므로 성능 제약 없음.
3. `agent`와 `admin-client`는 **네이티브 Tailscale 클라이언트**가 설치돼 있어야 한다 (Sunshine 영상 트래픽이 tailnet IP를 직접 써야 하므로 tsnet 사용 불가).
4. GUI 자동화 시 기본 디스패치는 **백그라운드**(대상 앱의 포커스를 뺏지 않음). 백그라운드 입력이 무시되는 앱일 경우에만 포그라운드로 폴백.
5. 파괴적 명령(재부팅, 프로세스 강제 종료, 파일 삭제 등)은 `admin-client`에서 사용자 확인 단계를 거친 뒤에만 `agent`에 전달한다.

---

## 1. `hub-server`

- **언어/스택**: Rust, `axum` + `tokio`
- **네트워킹**: tsnet — 후보 `tsnet` 크레이트(libtailscale 래핑) 또는 Tailscale 공식 Rust 프리뷰 중 선택. 프로토타입으로 비교 후 결정.
- **상태 저장**: 경량 임베디드 DB 필요 (예: `rusqlite`). 저장 대상: 노트북별 이름/MAC 매핑, 현재 세션 상태(누가 어느 노트북을 제어 중인지), 관리자 계정.
- **인증**: 관리자별 계정 필요 (다중 관리자 지원). 구체 방식(토큰/세션 쿠키 등)은 구현 시 결정.

### API 엔드포인트

| 메서드/경로 | 설명 |
|---|---|
| `POST /provision` | MAC 주소 기반 계정 이름 조회/발급. WTG `agent`의 최초 부팅 시 호출. 동일 MAC 재요청 시 같은 이름 반환. |
| `GET /agents` (또는 WS 구독) | 연결된 노트북 목록 + 실시간 상태(배터리 %, 디스크 여유공간, 프로세스 목록). Push 방식, 폴링 금지. |
| `POST /session/request` | 특정 노트북 제어 세션 요청. 인증 확인 + 동시 제어 충돌 검사(다른 관리자가 이미 점유 중이면 거부). 성공 시 대상 노트북의 Tailscale 주소 + 세션 토큰 반환. |
| `POST /session/release` | 세션 종료, 락 해제. |
| `POST /agents/{id}/command` | MCP 자동 진단 명령을 특정 agent에 전달 (hub가 중계, agent가 실행 후 결과를 hub에 보고 → 요청자에게 반환). |

---

## 2. `agent`

- **언어/스택**: Rust
- **네트워킹**: 네이티브 Tailscale 요구. authkey는 설정 파일 또는 환경변수로 주입 (하드코딩 금지).

### 최초 실행(프로비저닝) 로직

```
1. 네이티브 Tailscale 설치 여부 확인 (없으면 설치, 배포 형태에 따라 이미지/인스톨러에 이미 포함될 수도 있음)
2. `tailscale up --authkey=<authkey> --hostname=<generated> --advertise-tags=tag:camp-laptop` 비대화형 실행
3. 로컬 MAC 주소 수집 → hub-server `POST /provision` 호출 → 이름 수신
   - 네트워크/hub 응답 실패 시: 재시도 로직(지수 백오프) 후 fallback 이름(`student-temp-<random>`) 사용
4. (WTG 배포 한정) 수신한 이름으로 로컬 계정 생성 — 이 부분은 agent 바이너리가 아니라 별도 `provision.ps1`(specialize 스크립트)의 책임
```

### 상시 동작

- **상태 보고**: 배터리 %, 디스크 여유공간, 주요 프로세스 목록을 주기적으로 hub-server에 WebSocket push
- **명령 수신/실행** (hub-server로부터):
  - 일반 명령: PowerShell 프로세스 셸아웃 (`tokio::process::Command`) — WinRM 전용 크레이트는 아직 미성숙하므로 사용하지 않음
  - SSH가 필요한 드문 경우: Windows 내장 OpenSSH 클라이언트를 PowerShell로 호출(`run_powershell "ssh …"`) — 전용 SSH 경로/의존(russh)은 중복이라 제거함.
  - GUI 자동화(설치 마법사 등 CLI 없는 프로그램 대상): 스크린샷 캡처 + 클릭/타이핑. Windows에서는 UIA/MSAA 우선 시도 → 실패 시 좌표 기반 hit-test → 최종 폴백으로 Win32 입력. 디스패치는 기본 `PostMessage`(백그라운드), 대상 앱이 무시할 경우에만 `SendInput`(포그라운드)로 전환.
- **Sunshine 관리**: 별도 서브프로세스로 기동. Sunshine이 네이티브 Tailscale IP에서 직접 서비스되도록 구성(에이전트가 트래픽을 프록시하지 않음 — 3번 제약 참고).

### 배포 형태별 차이 (agent 코어 로직 외부)

- **WTG 내장형**: `unattend.xml` + `provision.ps1`(specialize 패스)로 이미지에 내장. 아래 "WTG 이미지 빌드" 섹션 참고.
- **일반 설치형**: 원안은 MSI 또는 NSIS 패키징이었으나 **채택하지 않았다**. 현행은 `deploy/install.ps1` 한 줄 부트스트랩(`irm <hub>/enroll/<시크릿> | iex`) + `kmc-agent-bundle.zip`(exe + ffmpeg DLL 8개) 전개 방식이다. 이유: (1) 설치 대상이 학생 노트북 수십 대라 갱신 주기가 짧은데 MSI 는 재서명·재배포 비용이 크다, (2) hub 가 `enroll` 응답으로 hub URL·authkey·릴리스 URL 을 주입하므로 인스톨러에 빌드타임 상수를 굽지 않아도 된다, (3) agent 본체는 `%LOCALAPPDATA%` 설치 + HKCU Run 등록이라 관리자 권한이 필요 없다. **단 Tailscale 은 예외** — WinTun 드라이버 + LocalSystem 서비스라 자체 MSI 를 UAC 승격으로 무인 설치한다(우리 패키징이 아니라 Tailscale 배포본이며, 배포 형태와 무관하게 회피 불가). 자세한 건 `deploy/README.md`.

---

## 3. `admin-client` (Tauri 데스크톱 앱)

- **스택**: Tauri (Rust 백엔드) + React + TypeScript + Vite (프론트엔드) + shadcn(Radix UI + Tailwind, CLI로 컴포넌트 추가)
- **네트워킹**: 네이티브 Tailscale 필요 (사용자 사전 설치 전제, 미설치 시 앱에서 안내만 표시)

### Rust 백엔드 책임

- hub-server와 HTTP/WebSocket 통신 (에이전트 목록/상태 구독, 세션 요청/해제, 명령 전달)
- `moonlight-common-rust`로 세션 승인 후 받은 노트북 Tailscale 주소에 직접 P2P 연결
- 디코딩된 영상 프레임을 `raw-window-handle`로 확보한 **네이티브 렌더링 서피스**에 직접 그림 (Tauri 웹뷰의 video/canvas 파이프라인 완전 우회 — 지연 최소화 목적)
- Sunshine 인코딩/캡처 코어는 재구현하지 않고 서브프로세스로 그대로 활용(FFI 아님, 프로세스 관리 + 기존 REST API/프로토콜로 제어)

### React 프론트엔드 책임

- shadcn 컴포넌트로 대시보드 구성:
  - `Table`: 노트북 목록 (이름, 상태, 배터리, 현재 제어 중인 관리자)
  - `Badge`: 온라인/오프라인, 배터리 위험 등 상태 표시
  - `Dialog`: 파괴적 명령 실행 전 확인
  - `Tabs`: MCP 자동 진단 화면 / 수동 원격제어(스트리밍) 화면 전환
  - `Sonner`(toast): 배터리 경고 등 실시간 알림
- 네이티브 스트리밍 서피스는 Rust 백엔드가 별도로 오버레이하므로, 프론트엔드는 해당 영역의 위치/크기만 관리(플레이스홀더)
- 모든 실시간 데이터는 WebSocket 구독으로 수신 (폴링 금지)

---

## Tailscale 설정

- **ACL 태그 3종**: `tag:hub`, `tag:admin`, `tag:camp-laptop`
- **ACL 규칙**:
  - `tag:camp-laptop` ← `tag:hub` 접근 허용 (관리 명령/상태 조회)
  - `tag:camp-laptop` ← `tag:admin` 접근 허용 (스트리밍/수동 제어)
  - `tag:camp-laptop` ↔ `tag:camp-laptop` 간 접근 차단
- **노드 유형**: `camp-laptop` 태그가 붙는 노드는 **영속(persistent) 노드**로 유지 — ephemeral authkey 사용 금지. Tailscale 상태 파일은 WTG 드라이브(또는 일반 설치형의 로컬 디스크)에 영속 저장하여 재부팅 시 동일 노드 정체성으로 재조인.
- **authkey**: 재사용 가능한 키 사용, 이미지/인스톨러에 하드코딩하지 않고 빌드 시점에 주입.

---

## WTG 이미지 빌드 스크립트 (산출물 2개)

### `unattend.xml`
- OOBE(EULA, 지역/키보드 설정, 계정 생성 화면) 전체 스킵 처리
- `specialize` 패스에 `provision.ps1` 실행 등록

### `provision.ps1` (specialize 패스에서 자동 실행)
```
1. 네트워크 연결 대기 (재시도 루프, DHCP 지연 고려)
2. 네이티브 Tailscale 설치 + `tailscale up --authkey=... --advertise-tags=tag:camp-laptop`
3. agent 실행 → hub-server `/provision` 호출 → 계정 이름 수신 (실패 시 fallback)
4. `New-LocalUser`로 로컬 계정 생성 (빈 비밀번호)
5. Winlogon 레지스트리에 자동 로그인 설정 (AutoAdminLogon, DefaultUserName)
6. 첫 로그인 후 웰컴 화면/OneDrive 팝업/Edge 첫 실행 화면 등 비활성화 레지스트리 값도 이미지에 사전 반영
```

### 이미지 빌드 순서 (수동 작업, 스크립트화 대상 아님)
```
기준 USB 세팅(위 스크립트 + 필요 프로그램 설치 완료 상태)
  → sysprep /oobe /generalize /shutdown
  → DISM /Capture-Image (WIM 캡처)
  → 나머지 USB에 DISM /Apply-Image
```

---

## 현재 구현 범위 밖 (설계만 반영)

- `web-client`, `streaming-bridge`: 브라우저는 tailnet 피어가 될 수 없으므로, 추후 구현 시 별도 네이티브 브릿지 프로세스(WebRTC/WebSocket ↔ 네이티브 Tailscale P2P 변환)가 필요. hub-server API는 지금부터 표준 HTTP/WS로 설계해 브라우저에서도 그대로 재사용 가능하게 유지할 것.
- 일반 설치형 배포 시 WTG의 물리적 디스크 격리 이점이 없으므로, 데이터 유실 방지가 필요하면 별도 요구사항(클라우드/외부 저장소 연동 등)으로 추가 논의 필요 — 현재 사양에는 미포함.

---

## 구현 현황 (2026-07, 자율 구현 세션)

실제 코드로 구현·검증된 것과 남은 것. 크레이트는 `kmc-*/` 디렉토리(독립 Cargo 패키지).

### ✅ 구현·검증 완료

| 영역 | 크레이트/산출물 | 검증 |
|---|---|---|
| 제어플레인 코어 | `kmc-proto`, `kmc-hub`(SQLite/WS/REST), `kmc-agent` | 로그인·에이전트목록·명령·세션충돌·저배터리알림 스모크 |
| 상태 보고 | agent `sysstat` (배터리/디스크/프로세스 WS push) | E2E |
| PowerShell 명령 | agent `exec.rs` (`CommandKind::PowerShell`) | E2E (파일수정·즉석Python실행 실증) |
| **스트리밍(자체구현)** | `kmc-streamhost`(GameStream 호스트), `kmc-moonclient`(클라), `kmc-admin`(Tauri) | 페어링 제거 후 WebCodecs GPU 디코드, 60fps E2E |
| **클라이언트 순수 Rust 전환 (moonlight-common-c FFI 제거)** | `kmc-gsproto`(호스트·클라 공유 와이어: RTP/NvVideoPacket/FEC 파라미터/제어 메시지·`parity_shard_count()`·`ports_from_base()`), `kmc-moonclient` 자체 구현 = RTSP 핸드셰이크(`rtsp.rs`) + ENet 제어채널·AES-GCM 입력(`control.rs`) + FEC 디코드 비디오 뎁패킷타이저(`video.rs`, fec-rs) + Opus RTP(`audio.rs`) + 세션 오케스트레이션(`session.rs`). `kmc-mooncommon`(186MB C 벤더+cmake/bindgen/libclang 빌드체인) 삭제 → admin 이 C 툴체인 없이 60초 빌드 | **라이브 E2E**(`cargo run --example stream_smoke`, 실제 streamhost 상대): 8초에 비디오 269 AU(키 2, SPS7/PPS8/IDR5 확인, 전부 self-framed Annex-B), 오디오 Opus 1602프레임(=200/s, 5ms), 첫 프레임 693ms. **admin 경로 라이브 검증**(`cargo test --test live_fanout`, `KMC_LIVE_HOST` 지정 시): `StreamState::start` → 순수 Rust 클라 → broadcast → 프론트가 붙는 그 로컬 WS 에서 비디오 30 AU(키 1, 프레이밍 위반 0) + Opus 30 프레임 수신. 유닛 107개(gsproto 14 + moonclient 68 + streamhost 25) |
| **스트리밍 지연/렌더 최적화** | ① WS 소켓 `TCP_NODELAY`(루프백 Nagle ~40ms 버퍼링 제거) ② 프론트 렌더: WebGPU `importExternalTexture`(VideoFrame zero-copy 샘플링, 2D `drawImage`의 YUV→RGB+업로드 제거) — `videoRenderer.ts`, WebGPU 미지원 시 2D 폴백 자동 ③ AU 버퍼 self-framed(`data[0]`=타입) 로 팬아웃 재복사 1회 제거 | 라이브 E2E (정상 렌더 + 지연 개선 실증). ws clone 제거는 tungstenite 0.24 `Binary(Vec<u8>)` API 제약으로 보류(뷰어 1명이라 프레임당 1복사) |
| **코덱: H.264 (HEVC 보류)** | streamhost `qsv.rs` `h264_qsv`(Main, veryfast, low_delay_brc, 네이티브 해상도+비트레이트 하한), `webserver.rs` H.264 단독 광고(codec_support 0x0001, MaxLumaPixelsHEVC=0), moonclient H.264 요청, 프론트 WebCodecs `avc1.*`(SPS 파싱) codedWidth 전체 그리기 | **라이브 E2E 검증**: 노트북(Intel Arc, 2880×1800 네이티브) admin 스트림 `codec=h264`, 캔버스 2880×1808, 전체 데스크톱 잘림 없이 표시(CDP 픽셀 샘플 전 영역 내용). **HEVC 보류**: hevc_qsv 가 이 GPU 에서 SPS conformance window 를 1280×720 으로 잘못 기록→디코더가 좌상단만 표시. H.264 는 정상 |
| 오디오 | streamhost WASAPI 루프백→Opus→RTP, 프론트 WebCodecs AudioDecoder | E2E (100 Opus 프레임) |
| **원격 입력(키보드+마우스)** | **별도 프로세스 sidecar `kmc-keyhook.exe`** 가 저수준 훅(WH_KEYBOARD_LL+WH_MOUSE_LL) 소유 → admin(Tauri/WebView2)이 포그라운드일 때 admin-내부 훅 콜백이 안 오는 WebView2 제약 회피(실측: 내부 0회 vs 별도 프로세스 227회). 게이트 = 스트리밍 AND admin포커스(WindowEvent::Focused) AND 마우스가 canvas 위(sidecar가 프론트 보고 화면 rect와 마우스 절대좌표 비교). 키/버튼 소유권 추적으로 DOWN 이 간 곳(로컬/원격)으로 UP 도 보내 경계 전환 시 stuck 방지 + 스트림 종료 시 release-all. stdin(s/f/r 토글·rect)·stdout(k/mm/mb/ms/h) 파이프 IPC | 라이브 E2E (키/마우스 원격, hover 게이트, 경계 stuck 해소 실증) |
| agent↔스트림 통합 | agent가 streamhost in-process 기동, hub가 peer IP→세션 주소 반환 | E2E |
| **MCP 서버(AI 자동진단)** | `kmc-mcp`(rmcp stdio) 9도구: `list_agents`/`run_powershell`/`run_powershell_all`(팬아웃)/`gui_action`/`gui_sequence` + 전용 브라우저 `web_open`/`web_read`/`web_click`/`web_type` | E2E (전 도구) |
| **GUI 자동화** | cua-driver(MIT) CLI 셸아웃 브리지 (`CommandKind::Gui`) — 백그라운드 조작 | E2E (list_windows/apps, notepad 조작) |
| **cua-driver 데몬 통합** | agent `cua.rs`가 데몬 수명주기 보장: startup `ensure_daemon()`(status→없으면 `serve` detached 기동) + `enable_autostart()`(로그온 스케줄 작업); `run_gui`는 "daemon not running" 감지 시 되살려 1회 재시도(자가교정); `browser::ensure()`도 window 조회 전 데몬 보장 | E2E (데몬 kill→startup 복구/mid-session 자동복구+재시도/autostart registered 모두 확인) |
| **브라우저 자동화(단일)** | provision이 학생 Chrome 바로가기를 `--remote-debugging-port=9222 --user-data-dir=C:\kmc\chrome-profile`로 통일 → "사용자 Chrome == AI가 CDP로 조작하는 Chrome". agent `browser.rs`+`kmc_ensure_browser`가 같은 포트/프로필 공유, MCP `web_open`/`web_read`/`web_click`(텍스트 매칭)/`web_type`이 ensure→bind→op를 내부 처리(스크린샷·좌표 없이 DOM, 토큰 최소) | E2E (example.com·iana·bing: open/read/click/type); provision 통일은 문법·바로가기 로직 검증 |
| WTG 배포 | `deploy/unattend.xml`, `deploy/provision.ps1`, `deploy/README.md` | 작성 완료 (실이미지 빌드 수동); **현재 보류 — 일반 설치형 우선** |
| **Tailscale (네이티브)** | **설치·`up` 은 전부 elevated 인스톨러 책임** — `install.ps1` 의 승격 자식이 MSI 무인설치(`TS_NOLAUNCH`/`ONBOARDING_FLOW=hide`/`UNATTENDEDMODE=always`) + `up --unattended` 까지 끝낸다. agent 런타임은 `tailscale.rs::wait_ready()` 로 `BackendState=Running` 을 최대 20s 대기만 하고 **`up` 을 호출하지 않는다**(커밋 `3b389e0`: 비관리자 `up` 은 기동마다 UAC/로그인 GUI 를 띄웠다). 자기 100.x 는 `self_ip()` 가 읽어 Hello `stream_addr` 로 보고(커밋 `a49177b`). ACL `deploy/tailscale-acl.hujson` | **실 tailnet E2E (2026-07-26 라이브 재확인)**: 한 줄 설치가 방화벽 선등록→MSI→정책키→`up`→`tailnet 연결됨: 100.127.176.115` 순서로 완주(30초), hub 가 agent 를 online 으로 승격하고 `session_request` 가 그 100.x 를 반환 |
| **일반 설치형 (irm\|iex)** | `deploy/install.ps1`(원격 한 줄 설치) + `deploy/build-release-bundle.ps1`(exe+ffmpeg DLL 번들). **방화벽 인바운드 규칙을 승격 자식이 선등록** — agent 가 streamhost 를 in-process 로 띄워 GameStream 6포트를 와일드카드 bind 하므로, 규칙이 없으면 첫 기동에 "네트워크 액세스 허용" 대화상자가 뜬다(학생 계정은 승인 불가 → 한 줄 설치가 깨짐). `kmc-agent-TCP/UDP` 를 program-scoped·port=Any·**`-Profile Any`** 로 박고, 과거 취소로 생긴 **Block 규칙을 먼저 제거**한다(Block 이 Allow 를 이김) | 스크립트 파싱 OK(부모+승격 자식 AST); **번들 자기완결 검증**(ffmpeg PATH 없이 agent 기동 = exit53 해소); **방화벽**: 대화상자가 만든 실규칙이 `TCP/UDP × port=Any × profile=Public 단독`임을 실측(= Private 망에서 스트리밍 사망) → `-Profile Any` 로 교정, Block 정리 쿼리는 대소문자 다른 경로로도 매칭 확인. 실 릴리스 다운로드·TS MSI 설치는 공개호스트·관리자 환경 필요 |

### 아키텍처 결정 (사양 대비 변경/구체화)
- **"MCP 기반 AI 자동진단" 실현**: hub를 `kmc-mcp`(rmcp)가 감싸 Claude에 노출. `run_powershell_all`이 온라인 노트북 전체 병렬 팬아웃 → "다수 컴퓨터를 AI로 한 번에 관리" 실현.
- **GUI 자동화는 cua-driver(trycua, MIT) 차용**: UIA 백그라운드 조작을 재구현하지 않고 CLI 셸아웃으로 브리지. 사양의 "PostMessage 백그라운드 우선" 원칙과 부합. agent가 `%LOCALAPPDATA%\Programs\Cua\...\cua-driver.exe` 자동 탐색.
  - **데몬 상시 보장(agent 통합)**: GUI/브라우저 자동화가 전부 cua-driver 데몬(`\\.\pipe\cua-driver`)을 거치므로 데몬이 죽으면 전 조작이 실패한다. agent `cua.rs`가 세 겹으로 보장 — (1) startup `ensure_daemon()`: `status`로 확인 후 없으면 `serve`를 `DETACHED_PROCESS`로 기동하고 준비 폴링, (2) `enable_autostart()`: `cua-driver autostart enable`로 로그온 스케줄 작업 등록(관리자 불필요), (3) 런타임 자가교정: `run_gui`가 호출 결과에서 "daemon is not running"을 감지하면 `ensure_daemon()` 후 1회 재시도해 호출자에게 실패를 노출하지 않음. `browser::ensure()`도 window 조회(cua list_windows) 전에 데몬을 보장.
- **브라우저 자동화는 단일 트랙(provision-time CDP Chrome = 학생 상용 브라우저)**: 초기엔 전용/사용자 투트랙을 검토했으나, 우리 대상은 provision으로 우리가 처음부터 세팅하는 관리형 노트북이라 단일화가 더 단순·강력. provision이 학생 Chrome 바로가기에 `--remote-debugging-port=9222 --user-data-dir=C:\kmc\chrome-profile`를 주입 → 학생의 실제 로그인/쿠키가 이 프로필에 쌓이고(=사용자 브라우저), AI는 CDP로 토큰 없이 조작(=전용). agent `browser.rs`(`kmc_ensure_browser`)와 바로가기가 동일 포트/프로필을 공유해 먼저 뜬 한 프로세스를 함께 쓴다. MCP `web_*`가 `start_session→kmc_ensure_browser→get_browser_state(bind)→browser_navigate/click/type` 다단계 CDP 오케스트레이션을 서버에서 처리하고 LLM엔 compact 결과(page/elements/outline)만 반환(플럼빙 은닉). `web_click`은 ref가 호출 간 새지 않게 **보이는 텍스트로 매칭**(자기완결), `web_type`은 `actions`에 `type` 포함 여부로 입력 가능 요소를 판정하고 field 힌트 실패 시 첫 입력란으로 폴백. **제약**: Chrome 136+(현재 ~150)는 '기본 프로필'에 `--remote-debugging-port`를 무시(쿠키 탈취 악용 차단)하므로 반드시 비-기본 `--user-data-dir` 필요 — 그래서 기존 기본 Chrome에 붙는 게 아니라 처음부터 전용 프로필을 상용으로 만든다. `gui_action`(UIA)은 브라우저 트랙이 아니라 **비-브라우저 네이티브 앱(설치 마법사 등)** 용 + 극단적 폴백으로 잔존.
  - **WTG 의존성 없음**: 브라우저 *제어 자체*는 provision/WTG와 무관하게 어디서나 동작한다 — agent `browser::ensure()`가 debug Chrome을 필요 시 스스로 spawn(고정 프로필+포트)하므로 일반 설치 노트북에서도 `web_*`가 즉시 동작(단, 그 프로필은 로그인이 없는 새 프로필). WTG 전용인 건 "사용자 상용 Chrome을 그 debug 프로필로 통일"하는 *바로가기 세팅*뿐. 이를 non-WTG에서도 재현하도록 agent가 startup에 `browser::unify()`를 수행 — `KMC_UNIFY_BROWSER=1`일 때만 per-user Chrome 바로가기(사용자 시작메뉴/바탕화면/작업표시줄 + 보장용 바탕화면 아이콘)를 동일 플래그로 패치(관리자 불필요). provision은 이 env를 세팅하고(WTG), 일반 인스톨러는 `KMC_UNIFY_BROWSER=1`만 주면 동일 결과.
- **셸/파일은 PowerShell 유지**(computer-server 미채용): 겹치는 Python 스택 배포 회피.
- **스트리밍 페어링 제거**: 자체 스택 + hub 세션 인증이라 PIN 페어링 불필요(호스트가 모든 클라 신뢰).

### ❌ 남은 것
- **Tailscale: agent 쪽은 인스톨러가 보증, admin 쪽 안내 UX 구현 완료(2026-07-26)** — 노트북(agent)은 `install.ps1` 이 MSI 설치 + `up --unattended` + 정책키(`UnattendedMode=always`/`OnboardingFlow=hide`)까지 박으므로 "미설치"가 사실상 발생하지 않고, 빠지는 3가지 경로(authkey 미제공 / UAC 거부 / MSI 다운로드 차단)는 인스톨러가 이미 "원격 스트리밍 불가, 제어는 hub 로 계속" 로 명시 경고한다. 빠질 수 있는 건 **admin PC 뿐**(`deploy/README.md:119` "사용자 사전 설치 전제"). 그래서 `kmc-admin` `start_stream` 실패 시 tailnet 원인을 판별해 덧붙인다(`lib.rs` `tailnet_hint`). **판정 방식**: UDP `connect` 로 패킷 없이 커널 라우팅만 질의해 소스 IP 를 본다. Windows Tailscale 은 100.64/10 전체가 아니라 **피어별 /32 + MagicDNS `100.100.100.100/32`** 만 깔므로(실측 `Get-NetRoute 100.*`), 가입 여부는 임의 100.x 가 아니라 MagicDNS 주소로 물어야 한다 — 실측: 미가입 판정용으로 `100.64.0.1` 을 쓰면 가입 상태에서도 기본 게이트웨이(192.168.10.20)로 떨어져 오탐. 오프라인 피어는 /32 경로가 남아 오탐 없음(실측). 두 원인을 분리 안내: ① 미가입 → `tailscale up --advertise-tags=tag:admin`, ② 가입했으나 해당 노드 미인지 → 노드 만료·삭제/ACL 확인. 테스트: CGNAT 경계 + 분기 결정표 + 라이브 프로브(`#[ignore]`) 4건 통과.
- **Tailscale ACL 실발효 미측정**: 연결규칙(노트북↔노트북 격리, admin→노트북 스트리밍 포트만 허용)이 실제로 먹는지는 따로 측정한 적 없음 — 정책 파일 `deploy/tailscale-acl.hujson` 은 작성됐고 hub/admin/노트북이 각각 별도 tailnet 노드로 라이브 동작 중이다.
- **web-client + streaming-bridge**: 사양상 범위 밖.
- **릴리스 번들: 갱신 완료(2026-07-26)** — `deploy/build-release-bundle.ps1` 재빌드 → `gh release upload v0.1.0 --clobber`. 배포 URL(`.../releases/latest/download/kmc-agent-bundle.zip`)에서 실제로 내려받아 검증: 다운로드본 69,117,843 B 가 로컬 zip 과 바이트 동일, 내부 `kmc-agent.exe`(9,030,656 B) 가 `target/release` 산출물과 바이트 동일, ffmpeg DLL 8 개 동봉. 직전 자산(07-24)에 빠져 있던 agent 커밋 6건(지연기반 혼잡제어, loss EWMA 프레임레이트 독립화, 순수 Rust GameStream 클라, agent 런타임 `tailscale up` 제거, IDR churn 감소, 포트 파생 공유) 반영됨. 참고 비대칭: `install.ps1` 은 raw GitHub 에서 직접 받으므로 push 즉시 반영되지만 zip 은 항상 재빌드·재업로드가 필요하다. 낡은 자산 `kmc-agent-bundle-v2..v6.zip`(07-22 반복본, 각 ~69MB)은 그대로 남아 있음 — 설치 경로는 참조하지 않는다. MSI/NSIS 는 미채용(불필요). WTG 는 보류.
- **`build-release-bundle.ps1` 의 `$PSScriptRoot` 버그(수정 완료)**: `[CmdletBinding()]` 가 있으면 **param 기본값 평가 시점에 `$PSScriptRoot` 가 비어 있다**(본문에서는 정상). 실측으로 격리: 기본값 `"$PSScriptRoot\..\x"` → `"\..\x"`. 그래서 README 가 안내하던 `powershell -File deploy\build-release-bundle.ps1`(인자 없이)이 **항상** `agent 미빌드: \..\kmc-agent\...` 로 죽었다. 스크립트 상대 경로 기본값을 본문에서 채우도록 수정, 인자 없이 실행해 정상 동작 확인.
