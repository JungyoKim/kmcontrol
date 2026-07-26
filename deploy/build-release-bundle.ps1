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
  [string]$AgentExe,
  [string]$FfmpegBin,
  [string]$Out
)

$ErrorActionPreference = 'Stop'

# [CmdletBinding()] 가 붙으면 param 기본값이 평가되는 시점에 $PSScriptRoot 가 아직 비어
# 있다(본문에서는 정상). 실측: 기본값 "$PSScriptRoot\..\x" -> "\..\x" 로 평가돼
# `powershell -File deploy\build-release-bundle.ps1` 이 항상 "agent 미빌드" 로 죽었다.
# 그래서 스크립트 상대 경로 기본값은 반드시 본문에서 채운다.
if (-not $AgentExe)  { $AgentExe  = Join-Path $PSScriptRoot '..\kmc-agent\target\release\kmc-agent.exe' }
if (-not $Out)       { $Out       = Join-Path $PSScriptRoot 'kmc-agent-bundle.zip' }
if (-not $FfmpegBin) {
  $FfmpegBin = if ($env:FFMPEG_DIR) { Join-Path $env:FFMPEG_DIR 'bin' } else { "$env:USERPROFILE\ffmpeg-7.1-shared\bin" }
}

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
