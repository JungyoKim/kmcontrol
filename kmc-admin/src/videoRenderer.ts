// VideoFrame 렌더러: WebGPU(importExternalTexture, zero-copy) 우선, 미지원 시 2D canvas 폴백.
//
// WebGPU 경로: VideoFrame 을 GPUExternalTexture 로 가져와(복사 없음) 풀스크린 삼각형에
// 샘플링해 그린다. 2D drawImage 대비 YUV→RGB 변환·GPU 업로드 오버헤드를 없앤다.
// GPUExternalTexture 는 호출 시점(=현재 태스크)에만 유효하므로 매 프레임 import + bind group 재생성.
//
// @webgpu/types 의존을 피하려고 WebGPU 핸들은 로컬에서 any 로 다룬다(로직 가독성 유지).

export interface VideoRenderer {
  readonly backend: "webgpu" | "2d";
  // VideoFrame 을 그리고 close 한다(항상 프레임 소유권을 가져가 close 보장).
  draw(frame: VideoFrame): void;
  dispose(): void;
}

// eslint-disable-next-line @typescript-eslint/no-explicit-any
type GPU = any;

const WGSL = /* wgsl */ `
@group(0) @binding(0) var samp: sampler;
@group(0) @binding(1) var tex: texture_external;

struct VSOut {
  @builtin(position) pos: vec4<f32>,
  @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VSOut {
  // 풀스크린 삼각형(3정점) — uv 는 0..1, y 뒤집어 텍스처 좌표계 맞춤.
  var p = array<vec2<f32>, 3>(
    vec2<f32>(-1.0, -1.0),
    vec2<f32>( 3.0, -1.0),
    vec2<f32>(-1.0,  3.0),
  );
  var uv = array<vec2<f32>, 3>(
    vec2<f32>(0.0, 1.0),
    vec2<f32>(2.0, 1.0),
    vec2<f32>(0.0, -1.0),
  );
  var o: VSOut;
  o.pos = vec4<f32>(p[i], 0.0, 1.0);
  o.uv = uv[i];
  return o;
}

@fragment
fn fs(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
  return textureSampleBaseClampToEdge(tex, samp, uv);
}
`;

async function createWebGpuRenderer(canvas: HTMLCanvasElement): Promise<VideoRenderer | null> {
  // navigator.gpu 는 표준 WebGPU 진입점. 타입 라이브러리 없이 in-guard 로 안전히 접근.
  if (!("gpu" in navigator)) return null;
  const gpu: GPU = navigator.gpu;
  if (!gpu) return null;
  const adapter = await gpu.requestAdapter();
  if (!adapter) return null;
  const device: GPU = await adapter.requestDevice();
  if (!device) return null;

  const ctx: GPU = canvas.getContext("webgpu");
  if (!ctx) return null;
  const format = gpu.getPreferredCanvasFormat();
  ctx.configure({ device, format, alphaMode: "opaque" });

  const module = device.createShaderModule({ code: WGSL });
  const pipeline: GPU = device.createRenderPipeline({
    layout: "auto",
    vertex: { module, entryPoint: "vs" },
    fragment: { module, entryPoint: "fs", targets: [{ format }] },
    primitive: { topology: "triangle-list" },
  });
  const sampler: GPU = device.createSampler({ magFilter: "linear", minFilter: "linear" });

  let disposed = false;

  return {
    backend: "webgpu",
    draw(frame: VideoFrame) {
      if (disposed) {
        frame.close();
        return;
      }
      try {
        // canvas 픽셀 크기를 프레임 코딩 해상도에 맞춘다(크롭 방지 — 2D 경로와 동일 규칙).
        const cw = frame.codedWidth || frame.displayWidth;
        const ch = frame.codedHeight || frame.displayHeight;
        if (canvas.width !== cw) canvas.width = cw;
        if (canvas.height !== ch) canvas.height = ch;

        // zero-copy: VideoFrame → GPUExternalTexture. bind group 은 매 프레임 새로.
        const external = device.importExternalTexture({ source: frame });
        const bind = device.createBindGroup({
          layout: pipeline.getBindGroupLayout(0),
          entries: [
            { binding: 0, resource: sampler },
            { binding: 1, resource: external },
          ],
        });
        const encoder = device.createCommandEncoder();
        const view = ctx.getCurrentTexture().createView();
        const pass = encoder.beginRenderPass({
          colorAttachments: [
            { view, clearValue: { r: 0, g: 0, b: 0, a: 1 }, loadOp: "clear", storeOp: "store" },
          ],
        });
        pass.setPipeline(pipeline);
        pass.setBindGroup(0, bind);
        pass.draw(3, 1, 0, 0);
        pass.end();
        device.queue.submit([encoder.finish()]);
      } catch {
        /* 프레임 단위 실패는 무시(다음 프레임 회복) */
      } finally {
        frame.close();
      }
    },
    dispose() {
      disposed = true;
      try {
        device.destroy?.();
      } catch {
        /* ignore */
      }
    },
  };
}

function create2dRenderer(canvas: HTMLCanvasElement): VideoRenderer {
  const ctx = canvas.getContext("2d");
  return {
    backend: "2d",
    draw(frame: VideoFrame) {
      try {
        const cw = frame.codedWidth || frame.displayWidth;
        const ch = frame.codedHeight || frame.displayHeight;
        if (canvas.width !== cw) canvas.width = cw;
        if (canvas.height !== ch) canvas.height = ch;
        ctx?.drawImage(frame as unknown as CanvasImageSource, 0, 0, cw, ch, 0, 0, cw, ch);
      } catch {
        /* ignore */
      } finally {
        frame.close();
      }
    },
    dispose() {
      /* 2D 컨텍스트는 별도 해제 불필요 */
    },
  };
}

// WebGPU 우선 생성, 실패하면 2D 폴백. 항상 유효한 렌더러를 반환.
export async function createVideoRenderer(canvas: HTMLCanvasElement): Promise<VideoRenderer> {
  try {
    const gpu = await createWebGpuRenderer(canvas);
    if (gpu) return gpu;
  } catch {
    /* WebGPU 초기화 실패 → 2D 폴백 */
  }
  return create2dRenderer(canvas);
}
