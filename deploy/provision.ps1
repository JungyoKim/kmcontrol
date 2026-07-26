<#
  kmc-agent WTG specialize-pass 프로비저닝 스크립트.

  사양서 §provision.ps1 요구를 순서대로 구현:
    1. 네트워크 연결 대기 (DHCP 지연 고려, 재시도 루프)
    2. 네이티브 Tailscale 설치 + `tailscale up --authkey=... --advertise-tags=tag:camp-laptop`
    3. agent 실행 → hub /provision 호출 → 계정 이름 수신 (실패 시 fallback)
    4. New-LocalUser로 로컬 계정 생성 (빈 비밀번호)
    5. Winlogon 레지스트리 자동 로그인 설정 (AutoAdminLogon/DefaultUserName)
    6. 웰컴/OneDrive/Edge 첫 실행 억제 레지스트리 반영

  비밀(authkey)은 하드코딩하지 않는다:
    - 빌드 시 이미지의 C:\kmc\provision.env 에 KMC_TS_AUTHKEY / KMC_HUB_URL 주입,
      또는 환경변수로 전달. 이 스크립트는 거기서 읽는다.

  로그: C:\kmc\provision.log
#>

$ErrorActionPreference = 'Continue'
$KmcDir = 'C:\kmc'
$LogPath = Join-Path $KmcDir 'provision.log'
function Log($m) { "$([DateTime]::UtcNow.ToString('s'))Z  $m" | Tee-Object -FilePath $LogPath -Append }

New-Item -ItemType Directory -Force -Path $KmcDir | Out-Null
Log "=== kmc provision start ==="

# ---- 설정 로드 (평문 하드코딩 금지) ----
$EnvFile = Join-Path $KmcDir 'provision.env'
$AuthKey = $env:KMC_TS_AUTHKEY
$HubUrl  = $env:KMC_HUB_URL
if (Test-Path $EnvFile) {
    Get-Content $EnvFile | ForEach-Object {
        if ($_ -match '^\s*([A-Z_]+)\s*=\s*(.+)$') {
            $k = $Matches[1]; $v = $Matches[2].Trim()
            if ($k -eq 'KMC_TS_AUTHKEY') { $AuthKey = $v }
            if ($k -eq 'KMC_HUB_URL')    { $HubUrl  = $v }
        }
    }
}
if (-not $HubUrl) { $HubUrl = 'http://127.0.0.1:8080' }
Log "hub_url=$HubUrl  authkey_present=$([bool]$AuthKey)"

# ---- 1. 네트워크 연결 대기 ----
$net = $false
for ($i = 0; $i -lt 30; $i++) {
    if (Test-Connection -ComputerName '1.1.1.1' -Count 1 -Quiet -ErrorAction SilentlyContinue) { $net = $true; break }
    Start-Sleep -Seconds 2
}
Log "network_ready=$net"

# ---- 2. Tailscale 설치 + up ----
# 모든 단계의 성패를 명시적으로 판정한다. 예전엔 다운로드/msiexec/up 어느 하나가 실패해도
# 조용히 통과해 "설치는 됐다는데 tailnet 에 안 붙은" 상태로 끝났다.
# (install.ps1 과 같은 로직. 둘 다 단독 실행 스크립트라 공유 모듈을 둘 수 없어 복제한다.)
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

# tailscaled 가 응답할 때까지 최대 $Seconds 대기. MSI 직후 서비스 등록·기동에 시간이 걸리는데
# 예전엔 대기가 아예 없어 곧바로 이어지는 up 이 조용히 죽었다.
function Wait-TsBackend([int]$Seconds) {
    for ($i = 0; $i -lt $Seconds; $i++) {
        if (Get-TsState) { return $true }
        Start-Sleep 1
    }
    return $false
}

if (-not (Test-Path $tsExe)) {
    $msi = Join-Path $KmcDir 'tailscale.msi'
    $url = 'https://pkgs.tailscale.com/stable/tailscale-setup-latest-amd64.msi'
    # 이미지에 미리 담아둔 MSI 가 있으면 그대로 쓴다(오프라인/느린 첫 부팅 대비).
    if ((Test-Path $msi) -and (Get-Item $msi).Length -lt 5MB) {
        Log "cached tailscale.msi too small ($((Get-Item $msi).Length) bytes) - discarding"
        Remove-Item $msi -ErrorAction SilentlyContinue
    }
    for ($try = 1; $try -le 3 -and -not (Test-Path $msi); $try++) {
        try {
            Log "downloading tailscale msi ($try/3)"
            Invoke-WebRequest -Uri $url -OutFile $msi -UseBasicParsing -TimeoutSec 180
            # MSI 는 수십 MB. 캡티브 포털/프록시가 끼워넣는 차단 페이지는 몇 KB 라 여기서 걸린다.
            $size = (Get-Item $msi).Length
            if ($size -lt 5MB) { throw "downloaded $size bytes - captive portal page?" }
        } catch {
            Log "tailscale download failed ($try/3): $_"
            Remove-Item $msi -ErrorAction SilentlyContinue
            Start-Sleep (2 * $try)
        }
    }
    if (Test-Path $msi) {
        Log "installing tailscale (msiexec /qn, GUI 억제)"
        # 0=성공, 3010=설치됨(재부팅 권고). 그 외는 실패인데 예전엔 종료코드를 안 봤다.
        # TS_NOLAUNCH 가 없으면 /qn 이어도 설치 끝에 트레이 GUI + 로그인 창이 뜬다
        # (실물 MSI 실행 시퀀스 조건이 `... AND (NOT TS_NOLAUNCH)`). 나머지 둘은
        # HKLM\SOFTWARE\Policies\Tailscale 정책으로 남아 온보딩을 막고 unattended 를 고정한다.
        $props = 'TS_NOLAUNCH=1 TS_ONBOARDING_FLOW=hide TS_UNATTENDEDMODE=always'
        $p = Start-Process msiexec.exe -ArgumentList "/i `"$msi`" /qn /norestart $props" -Wait -PassThru
        if ($p.ExitCode -notin 0, 3010) { Log "msiexec failed (exit=$($p.ExitCode))" }
    }
}
# MSI 프로퍼티는 "이미 설치돼 있어 MSI 를 건너뛴" 경로에 적용되지 않는다. provision 은
# 관리자로 돌므로 같은 값을 직접 박아 설치 경로와 무관하게 동일 상태로 만든다.
if (Test-Path $tsExe) {
    # EAP=Continue 라 -ErrorAction Stop 없이는 권한 거부가 non-terminating 오류로 흘러
    # catch 를 타지 않는다(실측: 키가 안 생기는데 로그는 조용함).
    try {
        $pol = 'HKLM:\SOFTWARE\Policies\Tailscale'
        if (-not (Test-Path $pol)) { New-Item -Path $pol -Force -ErrorAction Stop | Out-Null }
        Set-ItemProperty -Path $pol -Name 'UnattendedMode' -Value 'always' -Type String -ErrorAction Stop
        Set-ItemProperty -Path $pol -Name 'OnboardingFlow' -Value 'hide'   -Type String -ErrorAction Stop
        Log "tailscale policy set: UnattendedMode=always OnboardingFlow=hide"
    } catch { Log "tailscale policy registry failed (ignored): $_" }
}
if ((Test-Path $tsExe) -and $AuthKey) {
    if (-not (Wait-TsBackend 30)) { Log "tailscaled unresponsive after 30s - trying up anyway" }
    Log "tailscale up (advertise-tags=tag:camp-laptop, unattended)"
    # hostname 은 계정 이름을 받은 뒤 4b 에서 set 한다(여기선 아직 모른다).
    & $tsExe up --authkey=$AuthKey --advertise-tags=tag:camp-laptop --unattended 2>&1 | ForEach-Object { Log "ts: $_" }
    if ($LASTEXITCODE -ne 0) { Log "tailscale up failed (exit=$LASTEXITCODE) - authkey expired / tag not authorized?" }
    # 성공 기준은 "exe 가 있다"가 아니라 "tailnet 에 붙었다"이다. agent 의 Hello 가 100.x 를
    # 못 실으면 hub 가 프록시 내부 IP 로 폴백해 스트리밍이 통째로 깨진다.
    $state = $null
    for ($i = 0; $i -lt 20 -and $state -ne 'Running'; $i++) {
        $state = Get-TsState
        if ($state -ne 'Running') { Start-Sleep 1 }
    }
    if ($state -eq 'Running') { Log "tailscale connected ip=$((& $tsExe ip -4 2>$null | Select-Object -First 1))" }
    else { Log "tailscale NOT connected (BackendState=$state) - streaming unavailable" }
} else {
    Log "skip tailscale up (exe_present=$([bool](Test-Path $tsExe)) authkey_present=$([bool]$AuthKey))"
}

# 트레이 아이콘 제거. tailscaled 는 LocalSystem 서비스라 학생(비관리자)이 정지할 수 없지만,
# tailscale-ipn.exe 는 사용자 세션 프로세스라 눈에 띄고 종료할 수 있다. 종료해도 --unattended
# 라 tailnet 은 유지되므로(실측 확인) 아이콘만 없애면 된다. 자동시작 경로는 공용 시작폴더
# 바로가기 하나뿐이다. WTG 이미지는 학생에게 넘어가므로 여기선 항상 제거한다.
$tsLnk = Join-Path $env:ProgramData 'Microsoft\Windows\Start Menu\Programs\Startup\Tailscale.lnk'
if (Test-Path $tsLnk) { Remove-Item $tsLnk -Force -ErrorAction SilentlyContinue; Log "removed tailscale tray autostart" }
Get-Process tailscale-ipn -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue

# ---- 3. agent 실행 → /provision → 계정 이름 ----
# agent 바이너리는 이미지의 C:\kmc\kmc-agent.exe 에 배치. --provision-only 모드로 이름만 획득.
$agentExe = Join-Path $KmcDir 'kmc-agent.exe'
$accountName = $null
if (Test-Path $agentExe) {
    try {
        $mac = (Get-NetAdapter -Physical | Where-Object Status -eq 'Up' | Select-Object -First 1 -Expand MacAddress)
        if (-not $mac) { $mac = (Get-NetAdapter -Physical | Select-Object -First 1 -Expand MacAddress) }
        $body = @{ mac = $mac } | ConvertTo-Json
        $resp = Invoke-RestMethod -Uri "$HubUrl/provision" -Method POST -ContentType 'application/json' -Body $body -TimeoutSec 10
        $accountName = $resp.name
        Log "provisioned name=$accountName"
    } catch {
        Log "hub /provision failed: $_"
    }
}
if (-not $accountName) {
    $rand = -join ((48..57) + (97..102) | Get-Random -Count 6 | ForEach-Object { [char]$_ })
    $accountName = "student-temp-$rand"
    Log "fallback account name=$accountName"
}

# ---- 4. 로컬 계정 생성 (빈 비밀번호) ----
if (-not (Get-LocalUser -Name $accountName -ErrorAction SilentlyContinue)) {
    try {
        New-LocalUser -Name $accountName -NoPassword -AccountNeverExpires -ErrorAction Stop | Out-Null
        Add-LocalGroupMember -Group 'Users' -Member $accountName -ErrorAction SilentlyContinue
        Log "created local user $accountName"
    } catch { Log "New-LocalUser failed: $_" }
}

# ---- 4b. Tailscale hostname (계정 이름과 일치시켜 admin 목록에서 식별) ----
# `set --operator` 는 Linux 전용(비-root CLI 허용)이라 Windows 에선 무의미해 제거했다.
# Windows 는 tailscaled 가 LocalSystem 서비스고 비관리자도 status/ip 조회가 된다.
# up 을 두 번 부르지 않고 hostname 만 갱신한다.
if ((Test-Path $tsExe) -and (Get-TsState)) {
    & $tsExe set --hostname=$accountName 2>&1 | ForEach-Object { Log "ts-hostname: $_" }
    if ($LASTEXITCODE -ne 0) { Log "tailscale set --hostname failed (exit=$LASTEXITCODE)" }
    else { Log "tailscale hostname=$accountName" }
}

# ---- 5. 자동 로그인 (Winlogon) ----
$winlogon = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
Set-ItemProperty -Path $winlogon -Name 'AutoAdminLogon' -Value '1' -Type String
Set-ItemProperty -Path $winlogon -Name 'DefaultUserName' -Value $accountName -Type String
Set-ItemProperty -Path $winlogon -Name 'DefaultPassword' -Value '' -Type String
Set-ItemProperty -Path $winlogon -Name 'DefaultDomainName' -Value $env:COMPUTERNAME -Type String
Log "autologon configured for $accountName"

# ---- 6. 웰컴/OneDrive/Edge 첫 실행 억제 ----
$cvPolicies = 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\OOBE'
New-Item -Path $cvPolicies -Force | Out-Null
Set-ItemProperty -Path $cvPolicies -Name 'DisablePrivacyExperience' -Value 1 -Type DWord -ErrorAction SilentlyContinue
# OneDrive 자동 설치 억제
$odKey = 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\OneDrive'
New-Item -Path $odKey -Force | Out-Null
Set-ItemProperty -Path $odKey -Name 'DisableFileSyncNGSC' -Value 1 -Type DWord -ErrorAction SilentlyContinue

# ---- agent 자동 시작 등록 (로그온 세션에서 실행 — cua-driver GUI 조작에 필요) ----
# 로그온 세션에서 돌아야 하므로 HKCU Run 대신, 모든 사용자 로그온 시 실행되는 Run 키 사용.
$runKey = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Run'
if (Test-Path $agentExe) {
    Set-ItemProperty -Path $runKey -Name 'kmc-agent' -Value "`"$agentExe`"" -Type String
    Log "agent autostart registered"
}

# ---- 7. AI-제어용 CDP Chrome을 학생 상용 브라우저로 통일 ----
# Chrome 136+는 '기본 프로필'에 대해 --remote-debugging-port를 무시한다(인포스틸러의 쿠키 탈취 악용 차단).
# 따라서 전용(비-기본) 프로필을 쓰되, 그 프로필을 학생이 처음부터 쓰게 만들어
# "사용자 Chrome == AI가 CDP로 조작하는 Chrome"으로 단일화한다(투트랙 불필요).
# agent(browser.rs)와 바로가기가 동일 포트/프로필을 공유하면 먼저 뜬 한 프로세스를 함께 쓴다.
$ChromeProfile = 'C:\kmc\chrome-profile'
$CdpPort = 9222
New-Item -ItemType Directory -Force -Path $ChromeProfile | Out-Null
# 학생 계정(Users)이 이 프로필에 쓸 수 있도록 수정 권한 부여.
try { & icacls $ChromeProfile /grant "*S-1-5-32-545:(OI)(CI)M" /T 2>&1 | Out-Null } catch { Log "icacls chrome-profile: $_" }

# agent(browser.rs)가 같은 경로/포트를 쓰도록 머신 전역 환경변수 설정.
$envKey = 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Environment'
Set-ItemProperty -Path $envKey -Name 'KMC_BROWSER_PROFILE' -Value $ChromeProfile -Type String
Set-ItemProperty -Path $envKey -Name 'KMC_BROWSER_PORT' -Value "$CdpPort" -Type String
Set-ItemProperty -Path $envKey -Name 'KMC_UNIFY_BROWSER' -Value '1' -Type String
Log "browser env set profile=$ChromeProfile port=$CdpPort unify=1"

# 학생이 클릭하는 Chrome 바로가기에 debug 플래그를 주입(공용 위치).
$chromeExe = @(
    "$env:ProgramFiles\Google\Chrome\Application\chrome.exe",
    "${env:ProgramFiles(x86)}\Google\Chrome\Application\chrome.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if ($chromeExe) {
    $chromeArgs = "--remote-debugging-port=$CdpPort --user-data-dir=`"$ChromeProfile`" --no-first-run --no-default-browser-check"
    $wsh = New-Object -ComObject WScript.Shell
    # 기존 공용 바로가기 인자 덮어쓰기.
    @(
        'C:\ProgramData\Microsoft\Windows\Start Menu\Programs\Google Chrome.lnk',
        'C:\Users\Public\Desktop\Google Chrome.lnk'
    ) | ForEach-Object {
        if (Test-Path $_) {
            $sc = $wsh.CreateShortcut($_); $sc.Arguments = $chromeArgs; $sc.Save()
            Log "patched shortcut $_"
        }
    }
    # 보장용 공용 데스크톱 바로가기 생성.
    $desk = 'C:\Users\Public\Desktop\Chrome.lnk'
    $sc = $wsh.CreateShortcut($desk); $sc.TargetPath = $chromeExe; $sc.Arguments = $chromeArgs; $sc.Save()
    Log "created desktop shortcut $desk"
} else {
    Log "chrome.exe not found; skip shortcut patch"
}

Log "=== kmc provision done (account=$accountName) ==="
