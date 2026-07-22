# kmc 배포 (WTG / 일반 설치형)

사양서 §"WTG 이미지 빌드 스크립트" 및 §"배포 형태별 차이" 구현.
`agent` 코어 로직은 배포 형태와 무관하게 동일하며, 이 디렉토리는 **배포 래퍼**만 담는다.

## 산출물

| 파일 | 역할 |
|---|---|
| `unattend.xml` | OOBE 전체 스킵 + specialize 패스에 `provision.ps1` 등록 |
| `provision.ps1` | 네트워크 대기 → Tailscale 설치·up → hub `/provision` → 로컬 자동로그인 계정 → agent 자동시작 |

## 이미지에 미리 배치할 것

WTG USB(또는 golden WIM)의 `C:\kmc\` 에:
- `provision.ps1` (이 디렉토리)
- `kmc-agent.exe` (`cargo build --release -p kmc-agent`)
- `provision.env` — 비밀 주입 (하드코딩 금지):
  ```
  KMC_TS_AUTHKEY=tskey-auth-xxxxx   # 재사용 가능 authkey (tag:camp-laptop 발급 권한)
  KMC_HUB_URL=http://<hub-tailnet-ip>:8080
  ```
- (선택) cua-driver: agent가 GUI 자동화를 하려면 이미지에 cua-driver도 설치
  (`irm https://cua.ai/driver/install.ps1 | iex` 를 이미지 준비 단계에서 1회 실행).
  agent는 `%LOCALAPPDATA%\Programs\Cua\cua-driver\bin\cua-driver.exe` 를 자동 탐색
  (override: `KMC_CUA_DRIVER` 환경변수).

`unattend.xml` 은 이미지 루트(또는 `C:\Windows\Panther\unattend.xml`)에 배치.

## WTG 이미지 빌드 순서 (수동 — 사양서대로 스크립트화 대상 아님)

```
1. 기준 USB 세팅: Windows 설치 + C:\kmc\ 배치 + unattend.xml + 필요 프로그램
2. sysprep:   %WINDIR%\System32\Sysprep\sysprep.exe /generalize /oobe /shutdown /unattend:C:\kmc\unattend.xml
3. WinPE 부팅 후 캡처:   DISM /Capture-Image /ImageFile:D:\kmc.wim /CaptureDir:C:\ /Name:"kmc"
4. 나머지 USB에 적용:     DISM /Apply-Image /ImageFile:D:\kmc.wim /Index:1 /ApplyDir:W:\
                          bcdboot W:\Windows /s S: /f ALL
```

### WTG 주의 (스카우트 검증)
- WTG 이미지는 여러 하드웨어를 로밍하므로, 클래식 generalize 대신 **하드웨어 무관 첫 부팅**을 쓰는 게 일반적.
  즉 golden WIM을 DISM로 USB에 적용하고 첫 부팅 PnP가 드라이버를 잡게 한다.
- WTG USB 생성 도구: **Rufus**(GPL — 도구로만 사용, 코드 복사 금지)의 "Windows To Go" 모드,
  또는 위 DISM apply-to-USB 스크립트. (WTGA는 미유지; AOMEI/Hasleo는 폐쇄소스.)
- Win11을 임의 하드웨어에 올릴 때 TPM/SecureBoot 체크는 `unattend.xml` 의 windowsPE LabConfig로 우회.

## 일반 설치형 (권장 — PowerShell `irm | iex`)

WTG 대신 일반 Windows에 원격 한 줄 설치. WTG의 물리 디스크 격리 이점은 없지만 코어는 동일.

### 설치 산출물
| 파일 | 역할 |
|---|---|
| `install.ps1` | irm 진입점. 번들 다운로드→설치, cua-driver, (관리자 시)Tailscale, env+자동시작, agent 기동 |
| `build-release-bundle.ps1` | `kmc-agent.exe` + ffmpeg 런타임 DLL → `kmc-agent-bundle.zip` 생성기 |

### 릴리스 준비 (배포자, 1회)
```
cargo build --release -p kmc-agent      # FFMPEG_DIR 등 빌드 env 필요
powershell -File deploy\build-release-bundle.ps1   # → deploy\kmc-agent-bundle.zip
```
생성된 `kmc-agent-bundle.zip` 을 **공개 호스트(GitHub Releases 등)** 에 올리고,
`install.ps1` 의 `ReleaseUrl` 기본값(`https://github.com/OWNER/REPO/releases/latest/download/kmc-agent-bundle.zip`)을
실제 URL 로 바꾸거나 설치 시 `KMC_RELEASE_URL` 로 지정한다.

> **ffmpeg DLL 번들이 필수**: agent 는 streamhost(ffmpeg) 를 링크하므로 DLL 이 없으면 로더 단계에서
> 즉사(exit 53)한다. 번들은 DLL 을 exe 옆에 두어 PATH 조작 없이 로드되게 한다(검증됨).

### 노트북에서 설치 (관리자 PowerShell 권장)

**한 줄 설치** — 값이 없으면 스크립트가 대화형으로 물어본다(hub URL, authkey):
```powershell
irm https://<public-host>/install.ps1 | iex
```

무인/자동화라면 env 로 미리 지정(프롬프트 생략):
```powershell
$env:KMC_HUB_URL="http://<hub-tailnet-ip>:8080"; $env:KMC_TS_AUTHKEY="tskey-auth-..."; irm https://<public-host>/install.ps1 | iex
```

입력을 아예 없애는 3단계 UX:
1. **hub URL** 은 비밀이 아니므로 배포자가 `install.ps1` 의 `param($HubUrl=...)` 기본값에 박아두면 그 프롬프트가 사라진다.
2. **authkey** 만 남는데 — 대화형 붙여넣기(기본), env 지정, 또는
3. **완전 무입력**을 원하면 hub 가 콘솔에서 authkey 를 박은 개인화 스크립트를 발급하는 방식(Tailscale 콘솔의 "복사된 명령" 모델)으로 확장 가능(hub enroll 엔드포인트 필요, 미구현).
   → Claude Code 류의 무인자 한 줄은 "per-install 비밀이 없어서" 가능한 것이고, 우리는 tailnet authkey 라는 비밀이 있어 위 중 하나로 전달해야 한다.
- **무권한으로 되는 것**: agent(+DLL) 설치(`%LOCALAPPDATA%\kmc`), cua-driver, 사용자 env, `HKCU\...\Run` 자동시작, agent 기동.
- **관리자 필요**: Tailscale(서비스+WinTun 드라이버) 설치 + operator 지정. 관리자 PS 로 실행하면 원샷, 아니면 이 단계만 건너뛰고 안내(agent 는 LAN 동작, TS 는 나중에 관리자로).
- **네트워크 순서**: 공개 호스트에서 스크립트/번들 받기 → (authkey 로) Tailscale 부트스트랩 → agent 가 tailnet 으로 hub 연결. 그래서 번들은 tailnet-only hub 가 아니라 **공개 호스트**에 둔다.
- 갱신: 같은 명령 재실행(실행 중 agent 종료 후 덮어씀). 제거: `HKCU Run` 의 `kmc-agent` 삭제 + `%LOCALAPPDATA%\kmc` 삭제.

### 하드닝(선택)
- agent 를 `RUSTFLAGS=-C target-feature=+crt-static` 로 빌드하면 VC++ 재배포 의존이 사라져 더 자기완결적.

## Tailscale 설정 (네이티브 방식)

임베디드 tsnet 대신 **각 노드에 네이티브 tailscaled**를 쓴다(공식 `tailscale-rs`는 현재 Windows 미지원 + insecure preview). hub가 tailnet에서 도달되면 agent가 hub의 tailnet 주소로 WS를 맺고, hub가 그 연결의 peer_ip(=노드 100.x)를 세션 주소/스트리밍 타겟으로 그대로 쓴다 → 코드 변경 없이 tailnet-native.

### 콘솔 준비 (1회)
1. **ACL 정책 적용**: `deploy/tailscale-acl.hujson` 내용을 Tailscale 관리 콘솔 → Access Controls 에 붙여넣어 저장. 태그 `tag:hub`/`tag:admin`/`tag:camp-laptop` 과 연결방향 규칙이 정의돼 있다.
2. **재사용 authkey 발급**: Settings → Keys → *Reusable* + *Non-ephemeral* + `tag:camp-laptop` 부여. (영속 노드 요구 — ephemeral 금지.) → `provision.env` 의 `KMC_TS_AUTHKEY`.
3. **hub 노드**: hub 호스트에 tailscaled 설치 후 `tailscale up --advertise-tags=tag:hub`. hub는 그대로 8080 청취(0.0.0.0 또는 tailnet IF). `provision.env` 의 `KMC_HUB_URL` 을 hub 의 tailnet IP 로 지정.
4. **admin 노드**: 관리자 PC에 tailscaled 설치 + `tailscale up --advertise-tags=tag:admin` (사용자 사전 설치 전제).

### 노트북(agent) 측
- **설치**(admin 1회): Windows Tailscale 은 시스템 서비스 + WinTun 드라이버라 설치에 관리자 권한이 필수.
  - WTG: `provision.ps1` (SYSTEM) 이 MSI 무인 설치 + `tailscale up ... --advertise-tags=tag:camp-laptop --hostname=<계정>` + `tailscale set --operator=<계정>` 수행.
  - 비-WTG: MSI/NSIS 인스톨러(elevated)가 Tailscale MSI 를 chain-install + operator 지정.
- **런타임 연결**(비관리자): operator 로 지정된 학생 계정에서 agent `tailscale::ensure_up()` 가 startup 마다 연결을 자가보장(끊겼으면 `KMC_TS_AUTHKEY` 로 재-up). 미설치/키없음이면 graceful skip — 제어플레인은 LAN 으로 지속.
- override 환경변수: `KMC_TAILSCALE`(tailscale.exe 경로), `KMC_TS_AUTHKEY`(authkey).

> 설치 단계에만 admin 이 필요하고(1회), 이후 학생 계정 런타임은 operator 권한으로 무권한 연결/조회가 가능하다 (cua-driver 데몬과 동일한 "설치는 elevated, 운영은 agent" 분리).

## 패턴 출처 (MIT)
- [cschneegans/unattend-generator](https://github.com/cschneegans/unattend-generator) — specialize/OOBE/autologon XML 패턴
- [memstechtips/UnattendedWinstall](https://github.com/memstechtips/UnattendedWinstall) — 실제 autounattend.xml 스니펫
- Tailscale 무인 설치·`tailscale up --unattended` — Tailscale 공식 문서 (BSD-3)
- Winlogon 자동로그인 키 — Microsoft 공식 문서
