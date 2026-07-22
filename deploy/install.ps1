<#
  kmc-agent 일반 설치형 원격 설치 스크립트 (irm | iex).

  사용법 — 관리자 PowerShell 권장(Tailscale 설치에 관리자 필요; 나머지는 무권한 가능):

    $env:KMC_HUB_URL    = "http://<hub-tailnet-ip>:8080"
    $env:KMC_TS_AUTHKEY = "tskey-auth-..."     # (선택) 없으면 Tailscale 단계 생략, LAN으로 동작
    irm https://<public-host>/install.ps1 | iex

  또는 파라미터로:
    & ([scriptblock]::Create((irm https://<public-host>/install.ps1))) -HubUrl "..." -AuthKey "..."

  하는 일:
    1. kmc-agent + ffmpeg 런타임 DLL 번들 다운로드·설치 (%LOCALAPPDATA%\kmc, 무권한)
       — ffmpeg DLL 을 exe 옆에 두어 PATH 조작 없이 로드되게 함.
    2. cua-driver(GUI/브라우저 자동화 백엔드) 없으면 설치 시도 (무권한, best-effort)
    3. (authkey 제공 + 관리자) Tailscale 없으면 MSI 설치 + operator 지정
    4. agent 용 사용자 환경변수 + 로그온 자동시작(HKCU Run) 등록
    5. agent 즉시 기동
#>
[CmdletBinding()]
param(
  [string]$HubUrl     = $env:KMC_HUB_URL,
  [string]$AuthKey    = $env:KMC_TS_AUTHKEY,
  [string]$ReleaseUrl = $(if ($env:KMC_RELEASE_URL) { $env:KMC_RELEASE_URL } else { 'https://github.com/JungyoKim/kmcontrol/releases/latest/download/kmc-agent-bundle.zip' }),
  [string]$InstallDir = $(if ($env:KMC_INSTALL_DIR) { $env:KMC_INSTALL_DIR } else { "$env:LOCALAPPDATA\kmc" })
)

$ErrorActionPreference = 'Stop'
function Info($m) { Write-Host "[kmc] $m" -ForegroundColor Cyan }
function Warn($m) { Write-Host "[kmc] $m" -ForegroundColor Yellow }

# 값이 없으면 대화형으로 물어본다 → `irm .../install.ps1 | iex` 한 줄 설치 지원.
# env/param 은 자동화(무인 설치)용 override. 배포자가 param 기본값에 hub URL 을 박아두면 프롬프트도 생략됨.
if (-not $HubUrl)  { $HubUrl  = Read-Host 'hub URL (예: http://100.x.x.x:8080)' }
if (-not $HubUrl)  { throw 'hub URL 이 필요합니다.' }
if (-not $AuthKey) { $AuthKey = Read-Host 'Tailscale authkey (없으면 Enter=LAN 으로만 동작)' }

# ---- 1. agent 번들 다운로드·설치 (무권한) ----
Info "install dir: $InstallDir"
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
# Defender 제외 먼저(다운로드/압축 해제 전) — 서명 안 된 자체 빌드 exe 가 격리되는 것을 막는다.
# 관리자일 때만 가능. 다운로드 이전에 등록해야 방금 받은 exe 가 곧바로 격리되지 않는다.
$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltinRole]::Administrator)
if ($isAdmin) {
  try { Add-MpPreference -ExclusionPath $InstallDir -ErrorAction Stop; Info "Defender 제외 등록: $InstallDir" }
  catch { Warn "Defender 제외 실패(무시): $_" }
}
$zip = Join-Path $env:TEMP 'kmc-agent-bundle.zip'
Info "downloading bundle: $ReleaseUrl"
Invoke-WebRequest -Uri $ReleaseUrl -OutFile $zip -UseBasicParsing
# 실행 중인 agent 종료(파일 잠금 해제) 후 덮어쓰기.
Get-Process kmc-agent -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 500
Expand-Archive -Path $zip -DestinationPath $InstallDir -Force
Remove-Item $zip -ErrorAction SilentlyContinue
$agentExe = Join-Path $InstallDir 'kmc-agent.exe'
if (-not (Test-Path $agentExe)) { throw '번들에 kmc-agent.exe 가 없습니다.' }
Info "agent installed: $agentExe"

# ---- 2. cua-driver (무권한, best-effort) ----
$cua = "$env:LOCALAPPDATA\Programs\Cua\cua-driver\bin\cua-driver.exe"
if (-not (Test-Path $cua)) {
  $cuaUrl = $(if ($env:KMC_CUA_INSTALL_URL) { $env:KMC_CUA_INSTALL_URL } else { 'https://cua.ai/driver/install.ps1' })
  Info "installing cua-driver (GUI 자동화 백엔드): $cuaUrl"
  try { irm $cuaUrl | iex } catch { Warn "cua-driver 설치 실패(나중에 수동 설치 가능): $_" }
}

# ---- 3. Tailscale (authkey 제공 + 관리자) ----
# Windows 는 tailscaled 가 시스템 서비스라 --operator 불필요(Linux 전용). 관리자 컨텍스트에서
# 직접 `tailscale up --auth-key ... --unattended` 로 등록한다(agent ensure_up 에 안 맡김 —
# 비관리자 agent 가 up 하면 로그인 GUI 가 떠서 실패).
$tsExe = 'C:\Program Files\Tailscale\tailscale.exe'
if ($AuthKey) {
  if (-not (Test-Path $tsExe)) {
    if ($isAdmin) {
      Info 'installing Tailscale (MSI, 관리자)'
      $msi = Join-Path $env:TEMP 'tailscale.msi'
      Invoke-WebRequest -Uri 'https://pkgs.tailscale.com/stable/tailscale-setup-latest-amd64.msi' -OutFile $msi -UseBasicParsing
      Start-Process msiexec.exe -ArgumentList "/i `"$msi`" /qn /norestart" -Wait
      Remove-Item $msi -ErrorAction SilentlyContinue
    } else {
      Warn 'Tailscale 미설치 + 비관리자 → 설치 건너뜀. 관리자 PowerShell 로 재실행하세요. (agent 는 그동안 LAN 동작)'
    }
  }
  if ((Test-Path $tsExe) -and $isAdmin) {
    # MSI 직후 tailscaled 서비스가 뜰 시간을 준다(최대 ~20s).
    for ($i = 0; $i -lt 20; $i++) {
      try { & $tsExe status --json 2>$null | Out-Null; break } catch { Start-Sleep 1 }
    }
    Info 'tailscale up (authkey, tag:camp-laptop, unattended)'
    # 인자를 배열로 넘겨 PowerShell 의 -- 파싱 문제를 피한다.
    $up = @('up', "--auth-key=$AuthKey", '--advertise-tags=tag:camp-laptop', "--hostname=$env:COMPUTERNAME", '--unattended')
    & $tsExe @up 2>&1 | ForEach-Object { Info "ts: $_" }
  }
}

# ---- 4. 사용자 환경변수 + 자동시작 (무권한) ----
$stateFile = Join-Path $InstallDir 'agent-state.json'
[Environment]::SetEnvironmentVariable('KMC_HUB_URL', $HubUrl, 'User')
[Environment]::SetEnvironmentVariable('KMC_UNIFY_BROWSER', '1', 'User')      # 사용자 Chrome == AI 조작 Chrome 통일
[Environment]::SetEnvironmentVariable('KMC_CUA_DRIVER', $cua, 'User')
[Environment]::SetEnvironmentVariable('KMC_AGENT_STATE', $stateFile, 'User')
if ($AuthKey) { [Environment]::SetEnvironmentVariable('KMC_TS_AUTHKEY', $AuthKey, 'User') }

$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
Set-ItemProperty -Path $runKey -Name 'kmc-agent' -Value "`"$agentExe`"" -Type String
Info 'autostart registered (HKCU Run)'

# ---- 5. 즉시 기동 (현재 세션에도 env 반영) ----
$env:KMC_HUB_URL = $HubUrl
$env:KMC_UNIFY_BROWSER = '1'
$env:KMC_CUA_DRIVER = $cua
$env:KMC_AGENT_STATE = $stateFile
if ($AuthKey) { $env:KMC_TS_AUTHKEY = $AuthKey }
Start-Process -FilePath $agentExe -WindowStyle Hidden
Info "kmc-agent 기동 완료. hub=$HubUrl  dir=$InstallDir"
