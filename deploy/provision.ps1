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
$tsExe = 'C:\Program Files\Tailscale\tailscale.exe'
if (-not (Test-Path $tsExe)) {
    $msi = Join-Path $KmcDir 'tailscale.msi'
    if (-not (Test-Path $msi)) {
        try {
            Log "downloading tailscale msi"
            Invoke-WebRequest -Uri 'https://pkgs.tailscale.com/stable/tailscale-setup-latest-amd64.msi' -OutFile $msi -UseBasicParsing
        } catch { Log "tailscale download failed: $_" }
    }
    if (Test-Path $msi) {
        Log "installing tailscale (msiexec /qn)"
        Start-Process msiexec.exe -ArgumentList "/i `"$msi`" /qn /norestart" -Wait
    }
}
if ((Test-Path $tsExe) -and $AuthKey) {
    Log "tailscale up (advertise-tags=tag:camp-laptop, unattended)"
    & $tsExe up --authkey=$AuthKey --advertise-tags=tag:camp-laptop --unattended 2>&1 | ForEach-Object { Log "ts: $_" }
} else {
    Log "skip tailscale up (exe_present=$([bool](Test-Path $tsExe)) authkey_present=$([bool]$AuthKey))"
}

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

# ---- 4b. Tailscale operator + hostname (학생 계정이 런타임에 tailscale up/status 가능하도록) ----
# 설치·operator 지정은 여기(SYSTEM=admin)서 1회. 이후 agent(비관리자)가 ensure_up 으로 재연결 가능.
if (Test-Path $tsExe) {
    & $tsExe set --operator="$env:COMPUTERNAME\$accountName" 2>&1 | ForEach-Object { Log "ts-operator: $_" }
    if ($AuthKey) {
        & $tsExe up --authkey=$AuthKey --advertise-tags=tag:camp-laptop --hostname=$accountName --unattended 2>&1 | ForEach-Object { Log "ts-hostname: $_" }
    }
    Log "tailscale operator=$accountName hostname=$accountName"
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
