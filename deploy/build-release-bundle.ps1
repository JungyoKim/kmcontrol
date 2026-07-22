<#
  kmc-agent 릴리스 번들 생성기.

  kmc-agent.exe + ffmpeg 런타임 DLL(avcodec/avformat/avutil/swscale/… )을 한 zip 으로 묶는다.
  이 zip 을 GitHub Releases(또는 공개 호스트)에 `kmc-agent-bundle.zip` 이름으로 올리면
  install.ps1 이 받아 %LOCALAPPDATA%\kmc 에 풀고, DLL 이 exe 옆에 위치해 PATH 조작 없이 로드된다.

  사용:
    powershell -File deploy\build-release-bundle.ps1
    powershell -File deploy\build-release-bundle.ps1 -AgentExe <path> -FfmpegBin <dir> -Out <zip>
#>
[CmdletBinding()]
param(
  [string]$AgentExe  = "$PSScriptRoot\..\kmc-agent\target\release\kmc-agent.exe",
  [string]$FfmpegBin = $(if ($env:FFMPEG_DIR) { Join-Path $env:FFMPEG_DIR 'bin' } else { "$env:USERPROFILE\ffmpeg-7.1-shared\bin" }),
  [string]$Out       = "$PSScriptRoot\kmc-agent-bundle.zip"
)

$ErrorActionPreference = 'Stop'
if (-not (Test-Path $AgentExe))  { throw "agent 미빌드: $AgentExe (cargo build --release -p kmc-agent 먼저)" }
if (-not (Test-Path $FfmpegBin)) { throw "ffmpeg bin 없음: $FfmpegBin (FFMPEG_DIR 로 지정 가능)" }

$stage = Join-Path $env:TEMP 'kmc-bundle'
Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $stage | Out-Null

Copy-Item $AgentExe $stage
$dlls = Get-ChildItem (Join-Path $FfmpegBin '*.dll')
if (-not $dlls) { throw "ffmpeg DLL 을 못 찾음: $FfmpegBin\*.dll" }
Copy-Item $dlls.FullName $stage
Write-Host "[bundle] 포함: kmc-agent.exe + $($dlls.Count) DLL"

Remove-Item $Out -ErrorAction SilentlyContinue
Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $Out -Force
Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
Write-Host "[bundle] 생성 완료: $Out ($([Math]::Round((Get-Item $Out).Length/1MB,1)) MB)"
