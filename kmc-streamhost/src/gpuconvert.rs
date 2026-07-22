//! D3D11 VideoProcessor 기반 GPU 색변환 + 다운스케일.
//!
//! windows-capture가 주는 캡처 D3D11 텍스처(BGRA, 다운로드 없음)를 입력으로,
//! GPU에서 BGRA→NV12 변환 + 목표 해상도로 다운스케일한 뒤, 작아진 NV12만 CPU로 내려받는다.
//! (33MB 4K BGRA 다운로드 + CPU 색변환 병목 제거 — Sunshine VRAM 경로의 실용적 근사.)
//!
//! 입력 텍스처가 캡처 디바이스 소유이므로, 동일 ID3D11Device/Context를 재사용한다.

use anyhow::{anyhow, Result};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

/// GPU 변환기: 입력 4K BGRA → 출력 (tw×th) NV12 → CPU 스테이징.
pub struct GpuConverter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    processor: ID3D11VideoProcessor,
    enumerator: ID3D11VideoProcessorEnumerator,
    nv12_tex: ID3D11Texture2D,       // GPU 출력 NV12
    staging: ID3D11Texture2D,        // CPU 읽기용 NV12
    out_view: ID3D11VideoProcessorOutputView,
    tw: u32,
    th: u32,
    src_w: u32,
    src_h: u32,
    /// 입력 뷰 캐시 (동일 소스 텍스처면 재사용 불가 — 매 프레임 텍스처가 다를 수 있어 매번 생성).
    nv12_buf: Vec<u8>,
}

impl GpuConverter {
    /// 캡처 디바이스/컨텍스트와 소스·목표 해상도로 초기화.
    pub fn new(
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        src_w: u32,
        src_h: u32,
        tw: u32,
        th: u32,
    ) -> Result<Self> {
        unsafe {
            let video_device: ID3D11VideoDevice =
                device.cast().map_err(|e| anyhow!("ID3D11VideoDevice: {e}"))?;
            let video_context: ID3D11VideoContext =
                context.cast().map_err(|e| anyhow!("ID3D11VideoContext: {e}"))?;

            // VideoProcessor content desc: 입력 src, 출력 target, progressive.
            let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
                InputWidth: src_w,
                InputHeight: src_h,
                OutputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
                OutputWidth: tw,
                OutputHeight: th,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            let enumerator = video_device
                .CreateVideoProcessorEnumerator(&content_desc)
                .map_err(|e| anyhow!("create vp enumerator: {e}"))?;
            let processor = video_device
                .CreateVideoProcessor(&enumerator, 0)
                .map_err(|e| anyhow!("create vp: {e}"))?;

            // 색공간: 입력 RGB full-range, 출력 YCbCr BT.601 (Moonlight 기본).
            video_context.VideoProcessorSetStreamColorSpace(
                &processor,
                0,
                &D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                    _bitfield: 0, // RGB_Range=0(full), YCbCr matrix=0(BT.601), nominal=0
                },
            );
            video_context.VideoProcessorSetOutputColorSpace(
                &processor,
                &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 },
            );

            // 출력 NV12 텍스처 (GPU, VideoProcessor 출력 대상).
            let nv12_desc = D3D11_TEXTURE2D_DESC {
                Width: tw,
                Height: th,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut nv12_tex: Option<ID3D11Texture2D> = None;
            device
                .CreateTexture2D(&nv12_desc, None, Some(&mut nv12_tex))
                .map_err(|e| anyhow!("create nv12 tex: {e}"))?;
            let nv12_tex = nv12_tex.ok_or_else(|| anyhow!("nv12 tex null"))?;

            // CPU 읽기용 스테이징 텍스처.
            let staging_desc = D3D11_TEXTURE2D_DESC {
                Width: tw,
                Height: th,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut staging: Option<ID3D11Texture2D> = None;
            device
                .CreateTexture2D(&staging_desc, None, Some(&mut staging))
                .map_err(|e| anyhow!("create staging tex: {e}"))?;
            let staging = staging.ok_or_else(|| anyhow!("staging tex null"))?;

            // 출력 뷰.
            let out_view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut out_view: Option<ID3D11VideoProcessorOutputView> = None;
            video_device
                .CreateVideoProcessorOutputView(&nv12_tex, &enumerator, &out_view_desc, Some(&mut out_view))
                .map_err(|e| anyhow!("create output view: {e}"))?;
            let out_view = out_view.ok_or_else(|| anyhow!("output view null"))?;

            Ok(Self {
                device,
                context,
                video_device,
                video_context,
                processor,
                enumerator,
                nv12_tex,
                staging,
                out_view,
                tw,
                th,
                src_w,
                src_h,
                nv12_buf: vec![0u8; (tw * th * 3 / 2) as usize],
            })
        }
    }

    /// 소스 BGRA 텍스처를 GPU 변환·다운스케일해 NV12 바이트(타이트 팩)를 반환.
    pub fn convert(&mut self, src: &ID3D11Texture2D) -> Result<&[u8]> {
        unsafe {
            // 입력 뷰 생성 (소스 텍스처마다).
            let in_view_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
                },
            };
            let mut in_view: Option<ID3D11VideoProcessorInputView> = None;
            self.video_device
                .CreateVideoProcessorInputView(src, &self.enumerator, &in_view_desc, Some(&mut in_view))
                .map_err(|e| anyhow!("create input view: {e}"))?;
            let in_view = in_view.ok_or_else(|| anyhow!("input view null"))?;

            // 스트림: 입력 뷰 1개, 활성.
            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                OutputIndex: 0,
                InputFrameOrField: 0,
                PastFrames: 0,
                FutureFrames: 0,
                ppPastSurfaces: std::ptr::null_mut(),
                pInputSurface: std::mem::ManuallyDrop::new(Some(in_view.clone())),
                ppFutureSurfaces: std::ptr::null_mut(),
                ppPastSurfacesRight: std::ptr::null_mut(),
                pInputSurfaceRight: std::mem::ManuallyDrop::new(None),
                ppFutureSurfacesRight: std::ptr::null_mut(),
            };

            self.video_context
                .VideoProcessorBlt(&self.processor, &self.out_view, 0, &[stream])
                .map_err(|e| anyhow!("VideoProcessorBlt: {e}"))?;

            // NV12(GPU) → 스테이징 복사.
            self.context.CopyResource(&self.staging, &self.nv12_tex);

            // 스테이징 매핑해 타이트 팩으로 복사.
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| anyhow!("map staging: {e}"))?;

            let w = self.tw as usize;
            let h = self.th as usize;
            let row_pitch = mapped.RowPitch as usize;
            let base = mapped.pData as *const u8;
            // Y 평면: h 행.
            for y in 0..h {
                let src_row = base.add(y * row_pitch);
                let dst = &mut self.nv12_buf[y * w..y * w + w];
                std::ptr::copy_nonoverlapping(src_row, dst.as_mut_ptr(), w);
            }
            // UV 평면: NV12는 Y 다음 h/2 행. 스테이징에서 UV는 Y 영역 뒤(row_pitch 단위)에 이어짐.
            // NV12 텍스처 매핑: UV 시작 = pData + row_pitch * texture_height (align).
            let uv_offset = row_pitch * h;
            let uv_dst_off = w * h;
            for y in 0..(h / 2) {
                let src_row = base.add(uv_offset + y * row_pitch);
                let dst = &mut self.nv12_buf[uv_dst_off + y * w..uv_dst_off + y * w + w];
                std::ptr::copy_nonoverlapping(src_row, dst.as_mut_ptr(), w);
            }

            self.context.Unmap(&self.staging, 0);
            let _ = &self.device;
            let _ = (self.src_w, self.src_h);
            Ok(&self.nv12_buf)
        }
    }
}

// GpuConverter는 캡처 스레드에서만 사용됨 (COM, 단일 스레드).
unsafe impl Send for GpuConverter {}
