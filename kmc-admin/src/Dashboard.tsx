import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { toast } from "sonner";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Label } from "@/components/ui/label";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import type { AgentView, AlertPayload, CommandResult } from "@/types";

function fmtBytes(n: number): string {
  if (n <= 0) return "0";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = n;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(1)} ${units[i]}`;
}

export function Dashboard() {
  const [agents, setAgents] = useState<AgentView[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);

  useEffect(() => {
    const unlistenAgents = listen<AgentView[]>("agents", (e) => {
      setAgents(e.payload);
    });
    const unlistenAlert = listen<AlertPayload>("alert", (e) => {
      const a = e.payload;
      if (a.level === "critical") toast.error(a.message);
      else if (a.level === "warning") toast.warning(a.message);
      else toast.info(a.message);
    });
    return () => {
      unlistenAgents.then((f) => f());
      unlistenAlert.then((f) => f());
    };
  }, []);

  const selected = useMemo(
    () => agents.find((a) => a.agent_id === selectedId) ?? null,
    [agents, selectedId]
  );

  return (
    <div className="min-h-screen bg-background p-6">
      <h1 className="mb-4 text-2xl font-bold">KMC 관리 콘솔</h1>
      <Tabs defaultValue="mcp">
        <TabsList>
          <TabsTrigger value="mcp">MCP 자동 진단</TabsTrigger>
          <TabsTrigger value="manual">수동 원격제어</TabsTrigger>
        </TabsList>

        <TabsContent value="mcp">
          <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
            <Card>
              <CardHeader>
                <CardTitle>노트북 ({agents.length})</CardTitle>
              </CardHeader>
              <CardContent>
                <AgentTable
                  agents={agents}
                  selectedId={selectedId}
                  onSelect={setSelectedId}
                />
              </CardContent>
            </Card>
            <CommandPanel agent={selected} />
          </div>
        </TabsContent>

        <TabsContent value="manual">
          <ManualPanel agent={selected} agents={agents} onSelect={setSelectedId} />
        </TabsContent>
      </Tabs>
    </div>
  );
}

function AgentTable({
  agents,
  selectedId,
  onSelect,
}: {
  agents: AgentView[];
  selectedId: string | null;
  onSelect: (id: string) => void;
}) {
  return (
    <Table>
      <TableHeader>
        <TableRow>
          <TableHead>이름</TableHead>
          <TableHead>상태</TableHead>
          <TableHead>배터리</TableHead>
          <TableHead>디스크 여유</TableHead>
          <TableHead>제어 중</TableHead>
        </TableRow>
      </TableHeader>
      <TableBody>
        {agents.map((a) => {
          const batt = a.status?.battery_percent;
          const low = batt != null && batt < 20;
          return (
            <TableRow
              key={a.agent_id}
              onClick={() => onSelect(a.agent_id)}
              data-state={a.agent_id === selectedId ? "selected" : undefined}
              className="cursor-pointer"
            >
              <TableCell className="font-medium">
                {a.name}
                {a.status?.encoder_ok === false && (
                  <Badge
                    variant="destructive"
                    className="ml-2"
                    title="Intel QSV 하드웨어 인코더 사용 불가 — 그래픽 드라이버 업데이트 필요"
                  >
                    ⚠ 인코더
                  </Badge>
                )}
              </TableCell>
              <TableCell>
                <Badge variant={a.online ? "default" : "secondary"}>
                  {a.online ? "온라인" : "오프라인"}
                </Badge>
              </TableCell>
              <TableCell>
                {batt == null ? (
                  <Badge variant="outline">N/A</Badge>
                ) : (
                  <Badge variant={low ? "destructive" : "secondary"}>
                    {batt.toFixed(0)}%{a.status?.battery_charging ? " ⚡" : ""}
                  </Badge>
                )}
              </TableCell>
              <TableCell>
                {a.status ? fmtBytes(a.status.disk_free_bytes) : "-"}
              </TableCell>
              <TableCell>{a.controlled_by ?? "-"}</TableCell>
            </TableRow>
          );
        })}
        {agents.length === 0 && (
          <TableRow>
            <TableCell colSpan={5} className="text-center text-muted-foreground">
              연결된 노트북 없음
            </TableCell>
          </TableRow>
        )}
      </TableBody>
    </Table>
  );
}

function CommandPanel({ agent }: { agent: AgentView | null }) {
  const [script, setScript] = useState("Get-Process | Select-Object -First 5");
  const [destructive, setDestructive] = useState(false);
  const [result, setResult] = useState<CommandResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [confirmOpen, setConfirmOpen] = useState(false);

  async function execute() {
    if (!agent) return;
    setBusy(true);
    setError(null);
    setResult(null);
    try {
      const res = await invoke<CommandResult>("run_command", {
        agentId: agent.agent_id,
        script,
        destructive,
      });
      setResult(res);
    } catch (err) {
      setError(String(err));
    } finally {
      setBusy(false);
    }
  }

  function onRun() {
    if (destructive) {
      setConfirmOpen(true);
    } else {
      void execute();
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle>
          명령 실행{agent ? ` — ${agent.name}` : ""}
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-4">
        {!agent && (
          <p className="text-sm text-muted-foreground">
            좌측 표에서 노트북을 선택하세요.
          </p>
        )}
        <div className="space-y-2">
          <Label htmlFor="script">PowerShell 스크립트</Label>
          <Textarea
            id="script"
            value={script}
            onChange={(e) => setScript(e.target.value)}
            rows={4}
            disabled={!agent}
          />
        </div>
        <label className="flex items-center gap-2 text-sm">
          <input
            type="checkbox"
            checked={destructive}
            onChange={(e) => setDestructive(e.target.checked)}
            disabled={!agent}
          />
          파괴적 명령 (실행 전 확인)
        </label>
        <Button onClick={onRun} disabled={!agent || busy || !agent.online}>
          {busy ? "실행 중..." : "실행"}
        </Button>

        {error && <p className="text-sm text-destructive">{error}</p>}
        {result && (
          <div className="space-y-2">
            <p className="text-sm">
              종료 코드:{" "}
              <span className="font-mono">
                {result.exit_code ?? "N/A"}
              </span>
            </p>
            {result.error && (
              <p className="text-sm text-destructive">오류: {result.error}</p>
            )}
            {result.stdout && (
              <pre className="max-h-64 overflow-auto rounded bg-muted p-2 text-xs">
                {result.stdout}
              </pre>
            )}
            {result.stderr && (
              <pre className="max-h-40 overflow-auto rounded bg-destructive/10 p-2 text-xs text-destructive">
                {result.stderr}
              </pre>
            )}
          </div>
        )}
      </CardContent>

      <Dialog open={confirmOpen} onOpenChange={setConfirmOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>파괴적 명령 확인</DialogTitle>
            <DialogDescription>
              이 명령은 파괴적으로 표시되었습니다. {agent?.name} 에서 실행하시겠습니까?
            </DialogDescription>
          </DialogHeader>
          <pre className="max-h-40 overflow-auto rounded bg-muted p-2 text-xs">
            {script}
          </pre>
          <DialogFooter>
            <Button variant="outline" onClick={() => setConfirmOpen(false)}>
              취소
            </Button>
            <Button
              variant="destructive"
              onClick={() => {
                setConfirmOpen(false);
                void execute();
              }}
            >
              실행
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </Card>
  );
}

function ManualPanel({
  agent,
  agents,
  onSelect,
}: {
  agent: AgentView | null;
  agents: AgentView[];
  onSelect: (id: string) => void;
}) {
  const [msg, setMsg] = useState<string | null>(null);

  // 세션 요청 → hub가 반환한 호스트 주소. StreamView가 이 주소로 연결한다.
  async function acquire(): Promise<string> {
    if (!agent) throw new Error("노트북을 선택하세요");
    const addr = (await invoke("request_session", { agentId: agent.agent_id })) as string | null;
    if (!addr) throw new Error("hub가 호스트 주소를 반환하지 않음(에이전트 오프라인?)");
    setMsg(`세션 점유 — 호스트 ${addr}`);
    return addr;
  }

  async function release() {
    if (!agent) return;
    try {
      await invoke("release_session", { agentId: agent.agent_id });
      setMsg("세션 해제됨");
    } catch (err) {
      setMsg(String(err));
    }
  }

  return (
    <div className="space-y-4">
      <Card>
        <CardHeader>
          <CardTitle>수동 원격제어</CardTitle>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="space-y-2">
            <Label>대상 노트북</Label>
            <select
              className="w-full rounded border border-input bg-background p-2 text-sm"
              value={agent?.agent_id ?? ""}
              onChange={(e) => onSelect(e.target.value)}
            >
              <option value="">선택...</option>
              {agents.map((a) => (
                <option key={a.agent_id} value={a.agent_id}>
                  {a.name}
                </option>
              ))}
            </select>
          </div>
          {msg && <p className="text-sm text-muted-foreground">{msg}</p>}
        </CardContent>
      </Card>
      <StreamView agent={agent} acquire={acquire} release={release} />
    </div>
  );
}

// Annex-B H.264 바이트스트림에서 SPS(NAL type 7)를 찾아 WebCodecs codec 문자열
// "avc1.PPCCLL" (profile_idc/constraint_flags/level_idc, hex)을 만든다.
// 정확한 코덱 문자열이라야 VideoDecoder.configure가 하드웨어 디코더를 선택한다.
function codecFromSps(annexb: Uint8Array): string | null {
  const n = annexb.length;
  let i = 0;
  while (i + 4 < n) {
    if (annexb[i] === 0 && annexb[i + 1] === 0 && annexb[i + 2] === 1) {
      const nalStart = i + 3;
      const nalType = annexb[nalStart] & 0x1f;
      if (nalType === 7 && nalStart + 3 < n) {
        const hex = (x: number) => x.toString(16).padStart(2, "0");
        return `avc1.${hex(annexb[nalStart + 1])}${hex(annexb[nalStart + 2])}${hex(annexb[nalStart + 3])}`;
      }
      i = nalStart;
    } else {
      i++;
    }
  }
  return null;
}

// HEVC Main 프로파일, 레벨 5.1(=153, 4K 커버). streamhost hevc_qsv 출력(Main)과 일치.
const HEVC_CODEC = "hvc1.1.6.L153.B0";



function StreamView({
  agent,
  acquire,
  release,
}: {
  agent: AgentView | null;
  acquire: () => Promise<string>;
  release: () => Promise<void>;
}) {
  const [streaming, setStreaming] = useState(false);
  const [status, setStatus] = useState<string | null>(null);
  // 화질 프리셋 = 최대 "박스" 크기(긴 변 기준). agent 가 네이티브 화면비를 이 박스에 맞춰
  // 비율 유지로 축소(왜곡·업스케일 없음). "원본"은 박스 무제한(agent 네이티브 그대로).
  const QUALITY: Record<string, { w: number; h: number; label: string }> = {
    native: { w: 0, h: 0, label: "원본 (네이티브)" },
    high: { w: 2560, h: 2560, label: "고화질 (~2560)" },
    standard: { w: 1600, h: 1600, label: "표준 (~1600)" },
  };
  const [quality, setQuality] = useState<keyof typeof QUALITY>("high");
  const [hovering, setHovering] = useState(false); // sidecar 가 보고한 마우스 hover(캡처 표시용).
  const canvasRef = useRef<HTMLCanvasElement | null>(null);

  // 연결: 세션 요청(주소 획득) → 그 호스트로 스트림 시작. 노트북 선택 후 버튼 하나로 완결.
  async function start() {
    if (agent?.status?.encoder_ok === false) {
      setStatus(
        "이 노트북은 하드웨어 인코더(Intel QSV)를 쓸 수 없습니다 — Intel 그래픽 드라이버를 업데이트하세요 (10세대+ Intel GPU 필요).",
      );
      return;
    }
    setStatus("세션 요청 중...");
    try {
      const addr = await acquire();
      setStatus("연결 중...");
      // H.264 고정: hevc_qsv 가 이 GPU 에서 SPS crop 버그(화면 좌상단만 표시)라 HEVC 비활성.
      const allowHevc = false;
      const q = QUALITY[quality];
      // fps=0 = 무제한(agent 인코더 상한까지). width/height = 최대 박스(agent 가 네이티브 AR 유지 축소).
      await invoke("start_stream", {
        address: addr,
        width: q.w,
        height: q.h,
        fps: 0,
        pin: null,
        allowHevc,
      });
      setStreaming(true);
      setStatus(`스트리밍 중 — ${addr}`);
    } catch (err) {
      setStatus(`실패: ${String(err)}`);
    }
  }

  async function stop() {
    try {
      await invoke("stop_stream");
    } catch {
      /* ignore */
    }
    await release();
    setStreaming(false);
    setStatus("중지됨");
  }

  // 네이티브 경로: 호스트가 보낸 인코딩 H.264 access unit을 로컬 WS로 받아
  // WebCodecs VideoDecoder가 GPU로 디코드 → VideoFrame을 canvas에 GPU 합성.
  // (ffmpeg 소프트웨어 디코드/RGBA 복사 없음 — Moonlight 웹 클라이언트와 동일 방식.)
  useEffect(() => {
    if (!streaming) return;
    let alive = true;
    let ws: WebSocket | null = null;
    let decoder: VideoDecoder | null = null;
    let configured = false;
    let ts = 0;
    let gotFrame = false;
    let gotData = false;
    const canvas = canvasRef.current;
    const ctx = canvas?.getContext("2d") ?? null;

    const draw = (frame: VideoFrame) => {
      gotFrame = true;
      if (canvas && ctx) {
        // codedWidth/Height = 실제 인코딩 해상도(예 2880×1800). displayWidth/visibleRect 는
        // 일부 인코더(hevc_qsv)가 SPS conformance window 를 잘못 써 1280×720 등으로 축소 보고할 수 있어
        // 화면이 좌상단만 잘려 나온다. 전체 코딩 프레임을 그려 크롭을 방지한다.
        const cw = frame.codedWidth || frame.displayWidth;
        const ch = frame.codedHeight || frame.displayHeight;
        if (canvas.width !== cw) canvas.width = cw;
        if (canvas.height !== ch) canvas.height = ch;
        ctx.drawImage(
          frame as unknown as CanvasImageSource,
          0, 0, cw, ch,
          0, 0, cw, ch,
        );
      }
      frame.close();
    };

    const setup = async () => {
      if (!("VideoDecoder" in window)) {
        setStatus("이 WebView는 WebCodecs를 지원하지 않습니다");
        return;
      }
      const port = (await invoke("stream_port")) as number | null;
      if (!port || !alive) return;
      const streamCodec = ((await invoke("stream_codec")) as string) || "h264";
      decoder = new VideoDecoder({
        output: draw,
        error: (e) => console.error("VideoDecoder error", e),
      });
      ws = new WebSocket(`ws://127.0.0.1:${port}`);
      ws.binaryType = "arraybuffer";
      ws.onmessage = (ev) => {
        if (!alive || !decoder) return;
        gotData = true;
        const bytes = new Uint8Array(ev.data as ArrayBuffer);
        const key = bytes[0] === 1;
        const data = bytes.subarray(1);
        if (!configured) {
          if (!key) return; // 첫 키프레임 전의 델타는 버린다(디코더 동기).
          // HEVC 면 Main 프로파일 코덱 문자열(레벨 5.1=4K 커버), H.264 면 SPS 파싱값.
          const codec =
            streamCodec === "hevc"
              ? HEVC_CODEC
              : (codecFromSps(data) ?? "avc1.42E01F");
          try {
            decoder.configure({
              codec,
              optimizeForLatency: true,
              hardwareAcceleration: "prefer-hardware",
            } as VideoDecoderConfig);
          } catch (e) {
            console.error("configure failed", codec, e);
            return;
          }
          configured = true;
        }
        try {
          decoder.decode(
            new EncodedVideoChunk({ type: key ? "key" : "delta", timestamp: ts, data }),
          );
          ts += 16666; // 단조 증가 타임스탬프(μs, 대략 60fps).
        } catch (e) {
          console.error("decode failed", e);
        }
      };
    };
    setup();
    // 영상 무수신 진단: 연결 후 10초 안에 디코드된 프레임이 하나도 없으면 원인을 안내.
    const noVideoTimer = window.setTimeout(() => {
      if (alive && !gotFrame) {
        setStatus(
          gotData
            ? "영상 디코드 실패 — 코덱 호환 문제일 수 있습니다(콘솔 로그 확인)."
            : "영상 없음 — 호스트 인코더를 사용할 수 없습니다. Intel 그래픽 드라이버를 업데이트하세요 (10세대+ Intel GPU 필요).",
        );
      }
    }, 10000);

    return () => {
      alive = false;
      window.clearTimeout(noVideoTimer);
      try {
        ws?.close();
      } catch {
        /* ignore */
      }
      try {
        if (decoder && decoder.state !== "closed") decoder.close();
      } catch {
        /* ignore */
      }
    };
  }, [streaming]);

  // 원격 입력: 키보드/마우스는 네이티브 sidecar(kmc-keyhook.exe)가 저수준 훅으로 캡처한다
  // (WebView2 포커스 시 in-process 훅이 안 먹는 문제 회피). 프론트는 canvas 의 "화면 절대
  // 사각형(물리 px)"만 sidecar 에 보고하면, sidecar 가 마우스 hover 판정 + 좌표 변환을 한다.
  useEffect(() => {
    if (!streaming) return;
    const canvas = canvasRef.current;
    if (!canvas) return;
    // canvas 의 화면 절대 사각형(물리 px)을 계산해 sidecar 로 전달.
    // getBoundingClientRect 는 CSS px(뷰포트 기준) → 창 콘텐츠 원점(innerPosition, 물리 px) +
    // scaleFactor 로 화면 물리 좌표로 환산.
    const reportRect = async () => {
      try {
        const r = canvas.getBoundingClientRect();
        // 화면 절대 물리 px = (창 스크린 위치 + 뷰포트 내 CSS 좌표) * devicePixelRatio.
        // window.screenX/Y 는 브라우저(webview) 창의 스크린 좌표(CSS px). 순수 웹 API라 권한 불필요.
        const dpr = window.devicePixelRatio || 1;
        const originX = window.screenX + (window.outerWidth - window.innerWidth);
        const originY = window.screenY + (window.outerHeight - window.innerHeight);
        const l = Math.round((originX + r.left) * dpr);
        const t = Math.round((originY + r.top) * dpr);
        const rr = Math.round((originX + r.right) * dpr);
        const b = Math.round((originY + r.bottom) * dpr);
        await invoke("set_canvas_rect", { l, t, r: rr, b });
      } catch (e) {
        await invoke("set_canvas_rect", { l: -1, t: -1, r: -1, b: -1 }).catch(() => {});
        void e;
      }
    };

    // 초기 + 레이아웃 변화 시 갱신. 창 이동은 JS 이벤트가 없어 폴링으로 보완(가벼움).
    reportRect();
    const ro = new ResizeObserver(() => reportRect());
    ro.observe(canvas);
    window.addEventListener("resize", reportRect);
    window.addEventListener("scroll", reportRect, true);
    const poll = window.setInterval(reportRect, 500);

    // contextmenu 는 브라우저 기본 메뉴만 막는다(우클릭 자체는 sidecar 가 원격 전달).
    const onCtx = (e: MouseEvent) => e.preventDefault();
    canvas.addEventListener("contextmenu", onCtx);

    return () => {
      ro.disconnect();
      window.removeEventListener("resize", reportRect);
      window.removeEventListener("scroll", reportRect, true);
      window.clearInterval(poll);
      canvas.removeEventListener("contextmenu", onCtx);
      // rect 무효화(hover off) — 스트림 종료 시 캡처 안 되도록.
      invoke("set_canvas_rect", { l: 0, t: 0, r: 0, b: 0 }).catch(() => {});
    };
  }, [streaming]);

  // sidecar 가 보고하는 hover 상태를 배지에 반영(streaming 아닐 땐 false).
  useEffect(() => {
    if (!streaming) {
      setHovering(false);
      return;
    }
    const un = listen<boolean>("hover", (e) => setHovering(!!e.payload));
    return () => {
      un.then((f) => f()).catch(() => {});
    };
  }, [streaming]);

  // 오디오: ws로 Opus 프레임 수신 → WebCodecs AudioDecoder → Web Audio로 스케줄 재생.
  useEffect(() => {
    if (!streaming) return;
    let alive = true;
    let ws: WebSocket | null = null;
    let decoder: AudioDecoder | null = null;
    let ac: AudioContext | null = null;
    let playHead = 0; // 다음 버퍼를 넣을 AudioContext 시각(초).
    let gain: GainNode | null = null;
    let lastFade = 0; // 마지막 볼륨 램프 시각(초) — 리싱크 시 잦은 램프 방지.
    // 볼륨 램프: from→1 로 dur초. 초기/리싱크 글리치를 볼륨으로 감춘다(사용자 요청).
    const fadeUp = (from: number, dur: number, force = false) => {
      if (!ac || !gain) return;
      const t = ac.currentTime;
      if (!force && t - lastFade < 0.3) return;
      lastFade = t;
      gain.gain.cancelScheduledValues(t);
      gain.gain.setValueAtTime(Math.max(0.0001, from), t);
      gain.gain.linearRampToValueAtTime(1, t + dur);
    };

    const play = (ad: AudioData) => {
      if (!alive || !ac) {
        ad.close();
        return;
      }
      const frames = ad.numberOfFrames;
      const channels = ad.numberOfChannels;
      const buf = ac.createBuffer(channels, frames, ad.sampleRate);
      const tmp = new Float32Array(frames);
      for (let ch = 0; ch < channels; ch++) {
        ad.copyTo(tmp, { planeIndex: ch, format: "f32-planar" });
        buf.getChannelData(ch).set(tmp);
      }
      ad.close();
      const now = ac.currentTime;
      const TARGET = 0.01; // 목표 지터 버퍼 10ms — 최대한 타이트한 싱크.
      const MAX = 0.05; // 상한 50ms — 초과 시 드롭해 빠르게 지연 회수.
      if (playHead < now) {
        // 언더런(불연속) → 목표 리드로 리싱크 + 볼륨 살짝 죽였다 램프로 클릭 완화.
        playHead = now + TARGET;
        fadeUp(0.25, 0.1);
      } else if (playHead - now > MAX) {
        // 너무 앞섬(초기 버스트/드리프트) → 이 5ms 프레임 드롭 → 리드가 실시간으로 줄어 회수.
        return;
      }
      const src = ac.createBufferSource();
      src.buffer = buf;
      src.connect(gain ?? ac.destination);
      src.start(playHead);
      playHead += buf.duration;
    };

    const setup = async () => {
      if (!("AudioDecoder" in window)) return;
      const port = (await invoke("stream_audio_port")) as number | null;
      if (!port || !alive) return;
      ac = new AudioContext({ sampleRate: 48000 });
      try {
        await ac.resume();
      } catch {
        /* 사용자 제스처(연결 클릭)로 이미 허용됨 */
      }
      gain = ac.createGain();
      gain.connect(ac.destination);
      // 초기: 무음에서 0.5s 동안 서서히 키움 — 초기 버스트/드롭 글리치를 감춘다.
      gain.gain.setValueAtTime(0, ac.currentTime);
      gain.gain.linearRampToValueAtTime(1, ac.currentTime + 0.5);
      lastFade = ac.currentTime;
      decoder = new AudioDecoder({ output: play, error: (e) => console.error("AudioDecoder", e) });
      // Opus 48kHz 스테레오. Moonlight/Sunshine은 raw Opus 패킷을 보낸다.
      decoder.configure({ codec: "opus", sampleRate: 48000, numberOfChannels: 2 });
      ws = new WebSocket(`ws://127.0.0.1:${port}`);
      ws.binaryType = "arraybuffer";
      let ts = 0;
      ws.onmessage = (ev) => {
        if (!alive || !decoder || decoder.state !== "configured") return;
        const data = new Uint8Array(ev.data as ArrayBuffer);
        try {
          decoder.decode(new EncodedAudioChunk({ type: "key", timestamp: ts, data }));
          ts += 5000; // 5ms 프레임(μs).
        } catch (e) {
          console.error("audio decode failed", e);
        }
      };
    };
    setup();

    return () => {
      alive = false;
      try {
        ws?.close();
      } catch {
        /* ignore */
      }
      try {
        if (decoder && decoder.state !== "closed") decoder.close();
      } catch {
        /* ignore */
      }
      try {
        ac?.close();
      } catch {
        /* ignore */
      }
    };
  }, [streaming]);

  return (
    <Card>
      <CardHeader>
        <CardTitle>라이브 화면</CardTitle>
      </CardHeader>
      <CardContent className="space-y-3">
        <div className="flex items-end gap-2">
          {!streaming ? (
            <>
              <div className="flex flex-col gap-1">
                <label className="text-xs text-muted-foreground">화질</label>
                <select
                  value={quality}
                  onChange={(e) => setQuality(e.target.value as keyof typeof QUALITY)}
                  className="h-9 rounded-md border border-input bg-background px-2 text-sm"
                >
                  {Object.entries(QUALITY).map(([k, v]) => (
                    <option key={k} value={k}>
                      {v.label}
                    </option>
                  ))}
                </select>
              </div>
              <Button onClick={start} disabled={!agent}>
                연결
              </Button>
            </>
          ) : (
            <Button variant="outline" onClick={stop}>
              중지
            </Button>
          )}
        </div>
        {status && <p className="text-sm text-muted-foreground">{status}</p>}
        <div className="relative flex justify-center overflow-hidden rounded border border-border bg-black">
          {streaming ? (
            <>
              <canvas
                ref={canvasRef}
                tabIndex={0}
                className="block max-h-[75vh] w-auto min-h-0 min-w-0 max-w-full cursor-crosshair object-contain outline-none"
              />
              <div className="pointer-events-none absolute left-2 top-2 rounded px-2 py-1 text-xs font-medium"
                style={{ background: hovering ? "rgba(22,163,74,0.85)" : "rgba(0,0,0,0.6)", color: "white" }}>
                {hovering ? "입력 캡처 중 (마우스가 영상 위)" : "영상 위로 마우스를 올리면 입력 캡처"}
              </div>
            </>
          ) : (
            <div className="flex h-64 w-full items-center justify-center text-sm text-muted-foreground">
              연결하면 화면이 표시됩니다
            </div>
          )}
        </div>
      </CardContent>
    </Card>
  );
}
