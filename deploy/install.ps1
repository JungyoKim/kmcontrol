<#
  kmc-agent 일반 설치형 원격 설치 스크립트 (irm | iex).

  사용법 — 일반 PowerShell 로 실행해도 된다. 승격 단계에서만 UAC 승인 창이 한 번 뜨고,
  거부하면 그 단계만 건너뛴다(제어는 hub 로 계속 동작, 원격 스트리밍만 불가):

    $env:KMC_HUB_URL    = "http://<hub-tailnet-ip>:8080"
    $env:KMC_TS_AUTHKEY = "tskey-auth-..."     # (선택) 없으면 Tailscale 단계 생략, LAN으로 동작
    irm https://<public-host>/install.ps1 | iex

  또는 파라미터로:
    & ([scriptblock]::Create((irm https://<public-host>/install.ps1))) -HubUrl "..." -AuthKey "..."

  하는 일:
    1. (authkey 제공) **승격 자식 1회** — Defender 제외 + 방화벽 인바운드 규칙 + Tailscale
       MSI 설치 + `up --unattended`. 트레이 아이콘도 숨긴다(-KeepTailscaleTray 로 유지).
       로그: <InstallDir>\tailscale-setup.log
       다운로드보다 **먼저** 돌아야 한다 - Defender 제외가 늦으면 방금 받은 서명 없는 exe 가
       그 사이 격리된다. 권한이 필요한 작업은 전부 이 자식 하나에 모아 UAC 를 1회로 묶는다.
    2. kmc-agent + ffmpeg 런타임 DLL 번들 다운로드·설치 (%LOCALAPPDATA%\kmc, 무권한)
       — ffmpeg DLL 을 exe 옆에 두어 PATH 조작 없이 로드되게 함.
    3. cua-driver(GUI/브라우저 자동화 백엔드) 없으면 설치 시도 (무권한, best-effort)
    4. agent 용 사용자 환경변수 + 로그온 자동시작(HKCU Run) 등록
    5. agent 즉시 기동

  UAC 를 아예 안 뜨게 하려면 (셋 중 하나):
    a. **학생이 실제로 쓸 그 계정이 관리자**인 상태에서 "관리자 권한으로 PowerShell 실행" 후
       같은 한 줄을 돌린다. $isAdmin 분기가 승격 자식을 -Verb RunAs 없이 -NoNewWindow 로
       돌리므로 승인 창이 0회다. 다른 관리자 계정으로 승격하면 안 된다 - LOCALAPPDATA/HKCU 가
       그 계정으로 바뀌어 agent 가 엉뚱한 프로필에 깔린다(아래 승격 주석 참고).
    b. 이미지 단계에서 Tailscale 설치 + `up --unattended` 까지 끝내둔다(WTG provision.ps1 은
       specialize 패스 = SYSTEM 이라 UAC 개념 자체가 없다). 그러면 이 스크립트의 Tailscale
       단계가 전부 no-op 이 된다. 단 **MSI 만** 미리 깔아두는 건 부족하다 - 정책키(HKLM)와
       `up` 이 여전히 승격을 요구하므로 UAC 는 그대로 뜬다.
    c. authkey 를 주지 않는다(`if ($AuthKey)` 가드). 승격 자식 전체를 건너뛰어 UAC 가 0 회다.
       대신 Defender 제외·방화벽 규칙·Tailscale 이 모두 빠지고 원격 스트리밍도 없다(제어는
       hub 로 계속). 방화벽 규칙이 없어도 대화상자는 뜨지 않는다 - tailnet IP 가 없으면
       agent 가 streamhost 를 아예 기동하지 않아 포트를 bind 하지 않기 때문이다(main.rs).
  네이티브 Tailscale 을 쓰는 한 노트북당 UAC 1회는 회피 불가다 - WinTun 드라이버 + LocalSystem
  서비스라서다. agent 안에 Tailscale 을 임베드(tsnet/userspace-networking)하면 tailnet IP 가
  그 프로세스의 유저스페이스 netstack 안에만 존재해, OS 소켓으로 bind 하는 GameStream 6포트가
  패킷을 못 받는다(사양 제약 3번의 근거).
#>
[CmdletBinding()]
param(
  [string]$HubUrl     = $env:KMC_HUB_URL,
  [string]$AuthKey    = $env:KMC_TS_AUTHKEY,
  [string]$ReleaseUrl = $(if ($env:KMC_RELEASE_URL) { $env:KMC_RELEASE_URL } else { 'https://github.com/JungyoKim/kmcontrol/releases/latest/download/kmc-agent-bundle.zip' }),
  [string]$InstallDir = $(if ($env:KMC_INSTALL_DIR) { $env:KMC_INSTALL_DIR } else { "$env:LOCALAPPDATA\kmc" }),
  [switch]$KeepTailscaleTray
)

$ErrorActionPreference = 'Stop'
function Info($m) { Write-Host "[kmc] $m" -ForegroundColor Cyan }
function Warn($m) { Write-Host "[kmc] $m" -ForegroundColor Yellow }

# 값이 없으면 대화형으로 물어본다 → `irm .../install.ps1 | iex` 한 줄 설치 지원.
# env/param 은 자동화(무인 설치)용 override. 배포자가 param 기본값에 hub URL 을 박아두면 프롬프트도 생략됨.
if (-not $HubUrl)  { $HubUrl  = Read-Host 'hub URL (예: http://100.x.x.x:8080)' }
if (-not $HubUrl)  { throw 'hub URL 이 필요합니다.' }
if (-not $AuthKey) { $AuthKey = Read-Host 'Tailscale authkey (없으면 Enter=LAN 으로만 동작)' }

# ---- 1. 권한 필요 작업 (승격 1회): Defender 제외 + 방화벽 규칙 + Tailscale ----
Info "install dir: $InstallDir"
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
# agent 경로는 다운로드 전에 확정한다. 승격 자식이 Defender 제외와 방화벽 규칙을 이 경로로
# 미리 박아야 하기 때문이다 - 실제 파일 존재 확인은 압축 해제 후(2단계)에 한다.
$agentExe = Join-Path $InstallDir 'kmc-agent.exe'
# 학생 계정이 이미 관리자면 승격 자식을 UAC 없이 -NoNewWindow 로 돌린다(아래 분기 참고).
$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltinRole]::Administrator)

# 승격 자식 스크립트 본문 (Defender 제외 + 방화벽 규칙 + Tailscale).
# Windows 는 tailscaled 가 LocalSystem 서비스라 --operator 불필요(Linux 전용). 관리자
# 컨텍스트에서 `tailscale up --auth-key ... --unattended` 로 등록한다. agent 는 `up` 을 절대
# 부르지 않는다(비관리자 up = UAC/로그인 GUI → 기동마다 권한 창). --unattended 라 재부팅 후에도
# tailscaled 가 스스로 재연결하므로 런타임 재-up 자체가 불필요하다.
#
# 이 단계만 별도 스크립트로 떼어 필요할 때 UAC 승격한다. **스크립트 전체를 승격하면 안 된다** -
# UAC 에서 다른 관리자 계정으로 인증하면 $env:LOCALAPPDATA 와 HKCU 가 그 계정으로 바뀌어
# agent 가 엉뚱한 프로필에 설치된다. 번들·환경변수·자동시작은 학생 계정 컨텍스트가 필수다.
#
# 자식 스크립트는 모든 단계의 성패를 명시적으로 판정한다. 예전엔 다운로드/msiexec/up 어느
# 하나가 실패해도 조용히 통과해 "설치는 됐다는데 tailnet 에 안 붙은" 노트북이 나왔다.
$tsSetup = @'
param([Parameter(Mandatory)][string]$AuthKey, [string]$LogPath, [string]$AgentExe, [string]$InstallDir, [switch]$KeepTray)
$ErrorActionPreference = 'Continue'
function Log($m) {
  $line = "$([DateTime]::UtcNow.ToString('s'))Z  $m"
  # 부모가 로그 파일을 되읽어 출력한다. 파일에 썼으면 콘솔 출력은 생략 - 승격 자식은
  # 창이 숨겨져 있어 어차피 안 보이고, 관리자 직행 경로에선 중복 출력이 된다.
  $wrote = $false
  # -ErrorAction Stop 필수: 이 스크립트는 EAP=Continue 라 non-terminating 오류는 catch 를
  # 타지 않는다. 없으면 쓰기 실패에도 $wrote=$true 가 되어 출력이 통째로 사라진다.
  if ($LogPath) { try { Add-Content -LiteralPath $LogPath -Value $line -Encoding UTF8 -ErrorAction Stop; $wrote = $true } catch {} }
  if (-not $wrote) { Write-Host $line }
}

# ---- Defender 제외 (승격 필요) ----
# 서명 없는 자체 빌드 exe + 화면 캡처 + 입력 주입 + 원격 제어라 휴리스틱이 오탐하기 쉬운
# 조합이다. 격리되면 agent 가 조용히 죽고, 원격 제어 자체가 죽었으므로 원격으로 되살릴
# 수단이 없다(현장 방문). 부모가 이 자식을 다운로드 전에 호출하므로 여기서 등록하면
# "받자마자 격리" 창까지 닫힌다.
#
# 경로 한정 제외다 - InstallDir 밖에 설치되는 프로그램에는 아무 영향이 없다.
# Add-MpPreference 성공이 곧 반영은 아니다(관리 장비의 변조 방지는 제외 항목을 잠근다).
# 실제 반영 여부를 되읽어 판정한다 - 조용히 실패하면 격리를 뒤늦게 현장에서 발견하게 된다.
if ($InstallDir) {
  try {
    Add-MpPreference -ExclusionPath $InstallDir -ErrorAction Stop
    if (@((Get-MpPreference -ErrorAction Stop).ExclusionPath) -contains $InstallDir) {
      Log "Defender 제외 등록: $InstallDir"
    } else {
      Log "Defender 제외 미반영 (변조 방지로 잠겼을 수 있음): $InstallDir"
    }
  } catch { Log "Defender 제외 실패(무시): $_" }
}
$tsExe = 'C:\Program Files\Tailscale\tailscale.exe'

# BackendState 를 읽는다. 미설치/서비스 미기동/파싱 실패면 $null.
# 네이티브 exe 는 실패해도 throw 하지 않으므로 반드시 종료코드로 판정해야 한다.
function Get-TsState {
  if (-not (Test-Path $tsExe)) { return $null }
  try {
    $out = & $tsExe status --json 2>$null
    if ($LASTEXITCODE -ne 0) { return $null }
    return (($out -join "`n") | ConvertFrom-Json).BackendState
  } catch { return $null }
}

# tailscaled 가 응답할 때까지 최대 $Seconds 대기. MSI 직후 서비스 등록·기동에 시간이 걸린다.
# 예전 루프는 `try { & $tsExe status | Out-Null; break }` 였는데 네이티브 호출이 throw 하지
# 않아 첫 회에 무조건 break - 이름만 20초 대기였고 실제로는 0초였다(실측 0.02s).
function Wait-TsBackend([int]$Seconds) {
  for ($i = 0; $i -lt $Seconds; $i++) {
    if (Get-TsState) { return $true }
    Start-Sleep 1
  }
  return $false
}

# ---- 방화벽 인바운드 규칙 (승격 필요) ----
# kmc-agent 는 streamhost 를 in-process 로 띄우며 GameStream 포트 6개를 와일드카드로
# bind 한다(TCP 47984/47989/48010, UDP 47998/47999/48000). 규칙이 없으면 첫 기동 때
# Windows 가 "공용/개인 네트워크에서 이 앱에 액세스 허용" 대화상자를 띄운다. 학생 계정은
# 허용을 누를 수 없고(버튼이 승격을 요구), 무시/취소하면 Block 규칙이 박혀 스트리밍이
# 통째로 죽는다 - 그래서 한 줄 설치가 깨져 보였다.
#
# 대화상자를 이기는 게 아니라 앞지르는 것이다: 규칙이 먼저 있으면 프롬프트 자체가 없다.
# 게다가 대화상자보다 낫다 - 대화상자는 기본적으로 '개인' 프로필만 체크하므로 학교/카페
# 같은 '공용' 네트워크에서는 허용을 눌러도 인바운드가 막힌다. -Profile Any 로 박는다.
#
# -LocalPort 는 생략(=Any). 대화상자가 만들었을 규칙과 같은 형태이고, 포트 상수가 바뀌어도
# 규칙이 조용히 어긋나 프롬프트가 되살아나는 일이 없다.
# 이 자식은 다운로드보다 먼저 돌므로 exe 는 아직 없다. 방화벽은 존재하지 않는 경로도 규칙으로
# 받아준다(경로는 부모가 InstallDir 에서 확정해 넘긴다). Test-Path 가드를 두면 규칙이 통째로
# 빠져 첫 기동 때 대화상자가 되살아난다.
if ($AgentExe) {
  # 과거에 대화상자를 취소/무시했다면 Windows 가 이 프로그램 앞으로 Block 규칙을 박아둔다.
  # Windows 방화벽은 Block 이 Allow 를 이기므로, 걷어내지 않으면 아래 Allow 를 추가해도
  # 스트리밍이 계속 죽는다 - 정확히 이 증상을 겪은 노트북에서 고쳐지지 않는다는 뜻이다.
  # 이름이 아니라 "이 exe 를 가리키는 Block 인바운드 규칙" 전부를 대상으로 지운다.
  try {
    $ids = @(Get-NetFirewallApplicationFilter -ErrorAction Stop |
             Where-Object { $_.Program -and $_.Program -ieq $AgentExe } |
             ForEach-Object { $_.InstanceID })
    if ($ids.Count) {
      $stale = @(Get-NetFirewallRule -ErrorAction Stop |
                 Where-Object { $ids -contains $_.InstanceID -and $_.Direction -eq 'Inbound' -and $_.Action -eq 'Block' })
      foreach ($s in $stale) {
        Remove-NetFirewallRule -Name $s.Name -ErrorAction SilentlyContinue
        Log "방화벽 차단 규칙 제거: $($s.DisplayName)"
      }
    }
  } catch { Log "방화벽 차단 규칙 정리 실패(무시): $_" }

  foreach ($proto in 'TCP', 'UDP') {
    $rule = "kmc-agent-$proto"
    try {
      # 같은 이름의 과거 규칙(경로가 바뀐 재설치분)을 먼저 걷어내 멱등하게 만든다.
      Remove-NetFirewallRule -DisplayName $rule -ErrorAction SilentlyContinue
      New-NetFirewallRule -DisplayName $rule -Direction Inbound -Action Allow `
        -Program $AgentExe -Protocol $proto -Profile Any -ErrorAction Stop | Out-Null
      Log "방화벽 인바운드 허용: $rule -> $AgentExe"
    } catch { Log "방화벽 규칙 등록 실패($proto, 무시): $_" }
  }
} else {
  Log "방화벽 규칙 건너뜀 (AgentExe 없음: '$AgentExe')"
}

if (-not (Test-Path $tsExe)) {
  $msi = Join-Path $env:TEMP 'kmc-tailscale.msi'
  $url = 'https://pkgs.tailscale.com/stable/tailscale-setup-latest-amd64.msi'
  $got = $false
  for ($try = 1; $try -le 3 -and -not $got; $try++) {
    try {
      Log "downloading Tailscale MSI ($try/3)"
      Invoke-WebRequest -Uri $url -OutFile $msi -UseBasicParsing -TimeoutSec 180
      # MSI 는 수십 MB. 학교망 프록시/캡티브 포털이 끼워넣는 차단 페이지는 몇 KB 라 여기서
      # 걸린다(예전엔 그 HTML 을 msiexec 에 먹여 1620 으로 죽었다).
      $size = (Get-Item $msi).Length
      if ($size -lt 5MB) { throw "받은 파일이 $size bytes - 프록시 차단 페이지로 보인다" }
      $got = $true
    } catch {
      Log "Tailscale MSI 다운로드 실패 ($try/3): $_"
      Remove-Item $msi -ErrorAction SilentlyContinue
      Start-Sleep (2 * $try)
    }
  }
  if ($got) {
    Log 'installing Tailscale (msiexec /qn, GUI 억제)'
    # 0=성공, 3010=설치됨(재부팅 권고). 그 외는 실패 - 예전엔 종료코드를 안 봐서 1603/1618
    # 같은 실패에도 그대로 진행했다.
    #
    # TS_NOLAUNCH: 설치 끝에 트레이 GUI 를 띄우지 않는다. 실물 MSI(36MB, stable)의 실행
    # 시퀀스 조건이 `... AND (NOT TS_NOLAUNCH)` 라 신규 설치와 업그레이드 양쪽에 걸린다.
    # 이게 없으면 msiexec /qn 이어도 tailscale-ipn.exe 가 뜨고, 아직 up 전이라 로그인
    # 창까지 같이 떴다(우리 kill 은 up 이후라 그 사이 수십 초가 그대로 노출됐다).
    # TS_ONBOARDING_FLOW/TS_UNATTENDEDMODE 는 HKLM\SOFTWARE\Policies\Tailscale 정책으로
    # 박혀서, 학생이 GUI 를 직접 실행해도 온보딩이 안 뜨고 unattended 를 못 끈다.
    $props = 'TS_NOLAUNCH=1 TS_ONBOARDING_FLOW=hide TS_UNATTENDEDMODE=always'
    $p = Start-Process msiexec.exe -ArgumentList "/i `"$msi`" /qn /norestart $props" -Wait -PassThru
    if ($p.ExitCode -notin 0, 3010) { Log "msiexec 실패 (exit=$($p.ExitCode))" }
    Remove-Item $msi -ErrorAction SilentlyContinue
  }
}
if (-not (Test-Path $tsExe)) { Log 'Tailscale 설치 실패'; exit 3 }

# 위 정책은 MSI 프로퍼티라 "이미 설치돼 있어 MSI 를 건너뛴" 경로에는 적용되지 않는다.
# 승격된 상태이므로 같은 값을 직접 박아 설치 경로와 무관하게 동일 상태로 만든다.
# EAP=Continue 에서는 -ErrorAction Stop 이 없으면 권한 거부가 non-terminating 오류로 흘러
# catch 를 타지 않는다. 실측: 비관리자로 돌리면 키가 안 생기는데 로그는 조용했다.
try {
  $pol = 'HKLM:\SOFTWARE\Policies\Tailscale'
  if (-not (Test-Path $pol)) { New-Item -Path $pol -Force -ErrorAction Stop | Out-Null }
  Set-ItemProperty -Path $pol -Name 'UnattendedMode' -Value 'always' -Type String -ErrorAction Stop
  Set-ItemProperty -Path $pol -Name 'OnboardingFlow' -Value 'hide'   -Type String -ErrorAction Stop
  Log 'tailscale 정책 설정: UnattendedMode=always, OnboardingFlow=hide'
} catch { Log "정책 레지스트리 설정 실패(무시): $_" }

if (-not (Wait-TsBackend 30)) { Log 'tailscaled 가 30초 내 무응답 - up 을 그래도 시도한다' }
Log 'tailscale up (authkey, tag:camp-laptop, unattended)'
$up = @('up', "--auth-key=$AuthKey", '--advertise-tags=tag:camp-laptop', "--hostname=$env:COMPUTERNAME", '--unattended')
& $tsExe @up 2>&1 | ForEach-Object { Log "ts: $_" }
if ($LASTEXITCODE -ne 0) { Log "tailscale up 실패 (exit=$LASTEXITCODE) - authkey 만료/tag 권한 확인" }

# 트레이 아이콘 제거. tailscaled 는 LocalSystem 서비스라 학생(비관리자)이 정지할 수 없지만
# (실측: sc stop 거부), tailscale-ipn.exe 는 사용자 세션 프로세스라 눈에 띄고 종료할 수 있다.
# 종료해도 --unattended 라 tailnet 은 그대로 유지되므로(실측: kill 15초 뒤에도 Running +
# 100.x 유지) 아이콘만 없애면 된다. 자동시작 경로는 공용 시작폴더 바로가기 하나뿐이다.
if (-not $KeepTray) {
  $lnk = Join-Path $env:ProgramData 'Microsoft\Windows\Start Menu\Programs\Startup\Tailscale.lnk'
  if (Test-Path $lnk) { Remove-Item $lnk -Force -ErrorAction SilentlyContinue }
  Get-Process tailscale-ipn -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
  Log '트레이 아이콘 숨김(자동시작 제거 + 현재 인스턴스 종료)'
}

# 성공 기준은 "exe 가 있다"가 아니라 "tailnet 에 붙었다"이다. agent 의 Hello 가 100.x 를
# 못 실으면 hub 가 프록시 내부 IP 로 폴백해 스트리밍이 통째로 깨진다.
$state = $null
for ($i = 0; $i -lt 20 -and $state -ne 'Running'; $i++) {
  $state = Get-TsState
  if ($state -ne 'Running') { Start-Sleep 1 }
}
if ($state -ne 'Running') { Log "tailnet 미연결 (BackendState=$state)"; exit 2 }
Log "tailnet 연결됨: $(& $tsExe ip -4 2>$null | Select-Object -First 1)"
exit 0
'@

# authkey 가 없으면 승격 자체를 하지 않는다(UAC 0회 경로, 헤더 주석 c).
if ($AuthKey) {
  $tsLog  = Join-Path $InstallDir 'tailscale-setup.log'
  $tsFile = Join-Path $env:TEMP "kmc-ts-setup-$PID.ps1"
  Remove-Item $tsLog -ErrorAction SilentlyContinue
  # PS5.1 의 -Encoding UTF8 은 BOM 을 붙인다. powershell.exe -File 은 BOM 없는 UTF-8 을
  # ANSI(CP949)로 오독해 한글 문자열에서 구문 오류를 내므로 BOM 이 필요하다.
  # (이 install.ps1 자체는 `irm | iex` 로 실행돼 HTTP charset 으로 디코드되므로 BOM 금지.)
  Set-Content -LiteralPath $tsFile -Value $tsSetup -Encoding UTF8
  $psArgs = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$tsFile`"", '-AuthKey', "`"$AuthKey`"", '-LogPath', "`"$tsLog`"", '-AgentExe', "`"$agentExe`"", '-InstallDir', "`"$InstallDir`"")
  if ($KeepTailscaleTray) { $psArgs += '-KeepTray' }
  $exit = $null
  try {
    if ($isAdmin) {
      $exit = (Start-Process powershell.exe -ArgumentList $psArgs -Wait -PassThru -NoNewWindow).ExitCode
    } else {
      Info 'Defender 제외/방화벽 규칙/Tailscale 설치에 관리자 권한이 필요합니다 - UAC 승인 창이 한 번 뜹니다 (거부해도 나머지 설치는 계속됩니다)'
      # -WindowStyle Hidden: 승격 자식의 콘솔 창을 띄우지 않는다. UAC 동의 창은 남는다
      # (그건 의도된 것 - 사용자가 승인해야 한다). 진행 상황은 부모가 로그를 되읽어 보여준다.
      $exit = (Start-Process powershell.exe -ArgumentList $psArgs -Wait -PassThru -Verb RunAs -WindowStyle Hidden).ExitCode
    }
  } catch {
    # UAC 취소/거부는 여기로 온다(1223). 치명적이지 않다 - 제어 플레인은 hub 로 계속 동작한다.
    Warn "Tailscale 단계 건너뜀(권한 승격 실패/취소): $_"
  } finally {
    Remove-Item $tsFile -ErrorAction SilentlyContinue
  }
  # 자식은 Add-Content -Encoding UTF8 로 썼다. PS5.1 Get-Content 기본값은 ANSI 라
  # -Encoding UTF8 을 명시하지 않으면 한글이 깨진다.
  if (Test-Path $tsLog) { Get-Content -LiteralPath $tsLog -Encoding UTF8 | ForEach-Object { Info "ts: $_" } }
  switch ($exit) {
    0       { Info 'Tailscale OK (tailnet 연결됨)' }
    2       { Warn 'Tailscale 설치됐지만 tailnet 미연결 - 원격 스트리밍 불가. authkey/tag 권한을 확인하세요.' }
    3       { Warn 'Tailscale 설치 실패 - 원격 스트리밍 불가. 제어는 hub 로 계속 동작합니다.' }
    $null   { Warn '승격 단계 미실행 (Defender 제외·방화벽·Tailscale 모두 건너뜀) - 원격 스트리밍 불가. 제어는 hub 로 계속 동작합니다.' }
    default { Warn "Tailscale 설치 스크립트 비정상 종료 (exit=$exit)" }
  }
}

# ---- 2. agent 번들 다운로드·설치 (무권한) ----
# 승격 단계 뒤에 받는다 - Defender 제외가 먼저 등록돼야 압축 해제 직후 격리되지 않는다.
$zip = Join-Path $env:TEMP 'kmc-agent-bundle.zip'
Info "downloading bundle: $ReleaseUrl"
Invoke-WebRequest -Uri $ReleaseUrl -OutFile $zip -UseBasicParsing
# 실행 중인 agent 종료(파일 잠금 해제) 후 덮어쓰기.
Get-Process kmc-agent -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 500
Expand-Archive -Path $zip -DestinationPath $InstallDir -Force
Remove-Item $zip -ErrorAction SilentlyContinue
if (-not (Test-Path $agentExe)) { throw '번들에 kmc-agent.exe 가 없습니다.' }
Info "agent installed: $agentExe"

# ---- 3. cua-driver (무권한, best-effort) ----
$cua = "$env:LOCALAPPDATA\Programs\Cua\cua-driver\bin\cua-driver.exe"
if (-not (Test-Path $cua)) {
  $cuaUrl = $(if ($env:KMC_CUA_INSTALL_URL) { $env:KMC_CUA_INSTALL_URL } else { 'https://cua.ai/driver/install.ps1' })
  Info "installing cua-driver (GUI 자동화 백엔드): $cuaUrl"
  try { irm $cuaUrl | iex } catch { Warn "cua-driver 설치 실패(나중에 수동 설치 가능): $_" }
}

# ---- 4. 사용자 환경변수 + 자동시작 (무권한) ----
$stateFile = Join-Path $InstallDir 'agent-state.json'
[Environment]::SetEnvironmentVariable('KMC_HUB_URL', $HubUrl, 'User')
[Environment]::SetEnvironmentVariable('KMC_UNIFY_BROWSER', '1', 'User')      # 사용자 Chrome == AI 조작 Chrome 통일
[Environment]::SetEnvironmentVariable('KMC_CUA_DRIVER', $cua, 'User')
[Environment]::SetEnvironmentVariable('KMC_AGENT_STATE', $stateFile, 'User')
# authkey 는 설치 시점에만 쓰고 노트북에 남기지 않는다(agent 는 `up` 을 안 하므로 불필요).
# 과거 설치가 심어둔 값이 있으면 함께 제거한다.
[Environment]::SetEnvironmentVariable('KMC_TS_AUTHKEY', $null, 'User')

$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
Set-ItemProperty -Path $runKey -Name 'kmc-agent' -Value "`"$agentExe`"" -Type String
Info 'autostart registered (HKCU Run)'

# ---- 5. 즉시 기동 (현재 세션에도 env 반영) ----
$env:KMC_HUB_URL = $HubUrl
$env:KMC_UNIFY_BROWSER = '1'
$env:KMC_CUA_DRIVER = $cua
$env:KMC_AGENT_STATE = $stateFile
Start-Process -FilePath $agentExe -WindowStyle Hidden
Info "kmc-agent 기동 완료. hub=$HubUrl  dir=$InstallDir"
