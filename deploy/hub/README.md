# kmc-hub 를 dokploy(또는 임의 Docker PaaS)에 호스팅

hub 를 집 서버의 dokploy 에 올려 **공인 도메인 + HTTPS** 로 상시 노출한다.
그러면 Funnel/LAN 부트스트랩이 불필요 — enroll·provision·명령·상태 WS 가 전부 그 공개 URL 로 붙는다.

hub 는 순수 Rust + `rusqlite`(bundled, SQLite C 정적) 라 시스템 lib 의존이 없어 슬림 컨테이너로 뜬다.

## 무엇이 공개 hub 로 되고, 무엇이 안 되나
| 트래픽 | 경로 | 공개 hub |
|---|---|---|
| enroll / provision / 로그인 / 명령 / 상태 WS | agent·admin → hub | ✅ 완전 동작 |
| GUI·브라우저 자동화(명령의 일종) | 위와 동일 | ✅ |
| **스트리밍 P2P**(영상/오디오/입력) | admin ↔ agent **직접** | ⚠️ tailnet 주소 필요(아래) |

스트리밍 바이트는 hub 를 거치지 않는 **P2P** 다. 다만 admin 이 붙을 **주소**를 hub 가 알려주는데(`session_request`→`tailscale_addr`),
현재 그 주소를 "agent WS 연결의 출발지 IP"에서 뽑는다. 공개 hub 로 오면 그 값이 agent 의 **공인 NAT IP** 가 되어 P2P 도달이 안 된다.
→ 스트리밍까지 쓰려면 **2단계**(agent 가 `tailscale ip -4` 로 얻은 자기 100.x 를 status 로 보고 → hub 가 그걸 반환)가 필요. 제어플레인만 쓰면 지금 그대로 OK.

## dokploy 배포 순서

### 1. 앱 생성 (Application, Dockerfile 빌드)
- Source: 이 Git 저장소 (`https://github.com/JungyoKim/kmcontrol`), 브랜치 `main`
- Build Type: **Dockerfile**
- Docker File (Dockerfile Path): `deploy/hub/Dockerfile`
- **Docker Context Path (Build Path): `.`** — repo 루트여야 kmc-hub+kmc-proto 가 보인다. `/` 로 두면 컨텍스트가 비어 `COPY kmc-hub` 가 "not found" 로 실패한다. (`.` 이 안 먹으면 `./` 시도.)
- Exposed Port: **8080**

### 2. 볼륨 (DB 영속)
- Mount: 볼륨 → 컨테이너 경로 `/data`  (hub 는 `KMC_HUB_DB=/data/hub.db` 로 씀)
- 이걸 안 붙이면 재배포마다 등록/관리자/세션이 날아간다.

### 3. 환경변수 (Environment)
```
KMC_HUB_ADDR=0.0.0.0:8080
KMC_HUB_DB=/data/hub.db

# enroll (무입력 부트스트랩) — 원격 노트북이 irm https://<도메인>/enroll/<시크릿> | iex
KMC_ENROLL_SECRET=<추측 불가한 긴 시크릿>
KMC_ENROLL_AUTHKEY=tskey-auth-...            # Reusable+Non-ephemeral+tag:camp-laptop
KMC_ENROLL_HUB_URL=https://<hub-도메인>      # agent 가 붙을 hub 주소(= 이 공개 도메인)
KMC_RELEASE_URL=https://github.com/JungyoKim/kmcontrol/releases/latest/download/kmc-agent-bundle.zip
```
> 주의: 제어플레인만 쓸 땐 `KMC_ENROLL_HUB_URL` 을 **공개 도메인**으로. (스트리밍 2단계까지 가면 agent 가 tailnet 으로도 hub 에 닿을 수 있어야 하니 그때 재검토.)

### 4. 도메인 + HTTPS
- dokploy 에서 앱에 도메인 붙이면 Traefik 이 Let's Encrypt 로 HTTPS 종단.
- hub 는 컨테이너 안에서 평문 HTTP(8080) 로 두고, TLS 는 앞단이 처리(코드 변경 불필요).
- agent 는 `KMC_HUB_URL=https://<도메인>` 이면 WS 를 자동으로 `wss://` 로 승격(run.rs).

### 5. 최초 관리자 계정 (1회)
컨테이너 기동 후:
```
docker exec <컨테이너> kmc-hub admin add --username admin --password <강한 비밀번호>
```
(dokploy 의 터미널/exec 기능 사용.)

### 6. 스모크
```
curl https://<도메인>/enroll/<시크릿>        # install 원라이너가 text 로 나오면 성공
curl https://<도메인>/enroll/wrong           # 404 여야 함
```

## 노트북 등록 (원격, 관리자 PowerShell)
```powershell
irm https://<hub-도메인>/enroll/<시크릿> | iex
```
→ authkey+hub URL 이 주입된 install.ps1 이 실행되어: 번들 다운로드 → agent 설치(+ffmpeg DLL) → cua-driver → (관리자면)Tailscale 설치+operator+tag → 자동시작 → agent 기동 → hub 에 자기 MAC 으로 자동 등록(student-NN).

## MCP(관리자 측) 연결
`kmc-mcp` 의 `KMC_HUB_URL` 을 `https://<hub-도메인>` 으로, `KMC_MCP_USER`/`KMC_MCP_PASSWORD` 를 5번의 관리자 계정으로 지정하면 원격 hub 를 그대로 제어.
