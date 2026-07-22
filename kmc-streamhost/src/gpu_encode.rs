//! Zero-copy GPU 인코더: NV12 D3D11 텍스처(GpuConverter 출력, VRAM)를 CPU 다운로드 없이
//! h264_qsv/hevc_qsv 에 직접 먹인다. Sunshine 의 VRAM 경로와 동일한 아키텍처.
//!
//! 흐름(프레임당, 전부 GPU): GpuConverter NV12 텍스처 --CopySubresourceRegion--> ffmpeg D3D11
//! hwframe(RENDER_TARGET) --av_hwframe_map(DIRECT)--> QSV surface --avcodec_send_frame--> h264_qsv.
//! 우리 D3D11 디바이스(WGC 캡처 디바이스)를 ffmpeg d3d11va hwdevice 로 감싸고 QSV 를 파생시켜
//! 단일 디바이스로 동작(Arc 전용 노트북 = keyed-mutex 불필요).
//!
//! 검증: `hwmap=mode=direct:derive_device=qsv → h264_qsv` CLI 가 Intel QSV 에서 동작 확인.

extern crate ffmpeg_next as ffmpeg;

use anyhow::{anyhow, bail, Result};
use ffmpeg::ffi::*;
use std::ffi::{c_void, CString};
use ffmpeg::ffi::AVHWDeviceType::*;
use ffmpeg::ffi::AVPixelFormat::*;
use ffmpeg::ffi::AVPictureType::*;
use std::ptr;
use std::os::raw::c_int;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
};

use crate::qsv::EncodedPacket;

// ---- 하드-선언 D3D11VA 컨텍스트 구조체 (ffmpeg-sys bindgen 이 backend 헤더는 안 만들어줌) ----
// libavutil/hwcontext_d3d11va.h (ffmpeg 7.1) 레이아웃과 정확히 일치해야 함.
#[repr(C)]
struct AVD3D11VADeviceContext {
    device: *mut c_void,          // ID3D11Device*
    device_context: *mut c_void,  // ID3D11DeviceContext*
    video_device: *mut c_void,    // ID3D11VideoDevice*
    video_context: *mut c_void,   // ID3D11VideoContext*
    lock: Option<unsafe extern "C" fn(*mut c_void)>,
    unlock: Option<unsafe extern "C" fn(*mut c_void)>,
    lock_ctx: *mut c_void,
}
#[repr(C)]
struct AVD3D11VAFramesContext {
    texture: *mut c_void, // ID3D11Texture2D*
    bind_flags: u32,      // UINT BindFlags
    misc_flags: u32,      // UINT MiscFlags
}

fn averr(ret: c_int, ctx: &str) -> Result<()> {
    if ret < 0 {
        bail!("{ctx}: ffmpeg error {ret}");
    }
    Ok(())
}

/// 링 크기: 인코더 in-flight 프레임 수보다 크게(async_depth=1 → ~1-2). D3D11 풀(8)보다 작게.
const RING_SIZE: usize = 6;

/// 미리 매핑된 슬롯: D3D11 hwframe 와 그에 DIRECT 매핑된 QSV 프레임을 init 에서 한 번만 만들고
/// 재사용한다(프레임당 av_hwframe_map 제거 = Sunshine 방식). tex/index 는 CopySubresourceRegion 대상.
struct MappedSlot {
    d3d11_frame: *mut AVFrame,
    qsv_frame: *mut AVFrame,
    tex: *mut c_void, // ID3D11Texture2D*(배열)
    index: u32,       // subresource(배열 슬라이스) 인덱스
}

pub struct ZeroCopyEncoder {
    enc: *mut AVCodecContext,
    d3d11_dev: *mut AVBufferRef,
    qsv_dev: *mut AVBufferRef,
    d3d11_frames: *mut AVBufferRef,
    qsv_frames: *mut AVBufferRef,
    ring: Vec<MappedSlot>, // 미리 매핑된 프레임 링(재사용).
    ring_pos: usize,
    staged: usize,          // 직전 stage() 가 고른 링 슬롯(submit 에서 사용).
    ctx: ID3D11DeviceContext, // CopySubresourceRegion 용(우리 디바이스 컨텍스트)
    _device: ID3D11Device,    // 우리 ref 유지
    pkt: *mut AVPacket,
    pts: i64,
    width: u32,
    height: u32,
}

impl ZeroCopyEncoder {
    /// codec_name: "h264_qsv" | "hevc_qsv". device/ctx = WGC 캡처 D3D11(=GpuConverter 와 동일).
    /// out_w/out_h = 인코딩(=NV12 텍스처) 해상도.
    pub fn new(
        codec_name: &str,
        device: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        out_w: u32,
        out_h: u32,
        fps: u32,
        bitrate_bps: u32,
    ) -> Result<Self> {
        unsafe {
            // D3D11 디바이스에 멀티스레드 보호 설정(ffmpeg d3d11va 요구).
            if let Ok(mt) = device.cast::<ID3D11Multithread>() {
                let _ = mt.SetMultithreadProtected(true);
            }

            // 1) 우리 D3D11 디바이스를 ffmpeg d3d11va hwdevice 로 래핑.
            let d3d11_dev = av_hwdevice_ctx_alloc(AV_HWDEVICE_TYPE_D3D11VA);
            if d3d11_dev.is_null() {
                bail!("av_hwdevice_ctx_alloc(D3D11VA) failed");
            }
            let devctx = (*d3d11_dev).data as *mut AVHWDeviceContext;
            let d3dctx = (*devctx).hwctx as *mut AVD3D11VADeviceContext;
            // into_raw 로 소유 ref 를 ffmpeg 에 넘김(ffmpeg 이 free 시 Release).
            (*d3dctx).device = device.clone().into_raw();
            (*d3dctx).device_context = ctx.clone().into_raw();
            averr(av_hwdevice_ctx_init(d3d11_dev), "d3d11 device init")?;

            // 2) QSV 디바이스 파생(동일 GPU 자원 공유).
            let mut qsv_dev: *mut AVBufferRef = ptr::null_mut();
            averr(
                av_hwdevice_ctx_create_derived(&mut qsv_dev, AV_HWDEVICE_TYPE_QSV, d3d11_dev, 0),
                "derive qsv device",
            )?;

            // 3) D3D11 frames ctx (RENDER_TARGET → VideoProcessor/CopySubresource 가 쓸 수 있음).
            let d3d11_frames = av_hwframe_ctx_alloc(d3d11_dev);
            if d3d11_frames.is_null() {
                bail!("alloc d3d11 frames ctx failed");
            }
            let fctx = (*d3d11_frames).data as *mut AVHWFramesContext;
            (*fctx).format = AV_PIX_FMT_D3D11;
            (*fctx).sw_format = AV_PIX_FMT_NV12;
            (*fctx).width = out_w as c_int;
            (*fctx).height = out_h as c_int;
            (*fctx).initial_pool_size = (RING_SIZE + 2) as c_int;
            let fhw = (*fctx).hwctx as *mut AVD3D11VAFramesContext;
            (*fhw).bind_flags = D3D11_BIND_RENDER_TARGET.0 as u32;
            averr(av_hwframe_ctx_init(d3d11_frames), "d3d11 frames init")?;

            // 4) QSV frames ctx 를 D3D11 frames 로부터 DIRECT 매핑 파생.
            let mut qsv_frames: *mut AVBufferRef = ptr::null_mut();
            averr(
                av_hwframe_ctx_create_derived(
                    &mut qsv_frames,
                    AV_PIX_FMT_QSV,
                    qsv_dev,
                    d3d11_frames,
                    AV_HWFRAME_MAP_DIRECT as c_int,
                ),
                "derive qsv frames",
            )?;

            // 5) 인코더 설정(raw). hw_frames_ctx = QSV frames.
            let cname = CString::new(codec_name)?;
            let codec = avcodec_find_encoder_by_name(cname.as_ptr());
            if codec.is_null() {
                bail!("{codec_name} not found");
            }
            let enc = avcodec_alloc_context3(codec);
            if enc.is_null() {
                bail!("alloc codec ctx failed");
            }
            (*enc).width = out_w as c_int;
            (*enc).height = out_h as c_int;
            (*enc).time_base = AVRational { num: 1, den: fps as c_int };
            (*enc).framerate = AVRational { num: fps as c_int, den: 1 };
            (*enc).pix_fmt = AV_PIX_FMT_QSV;
            (*enc).bit_rate = bitrate_bps as i64;
            (*enc).rc_max_rate = bitrate_bps as i64;
            (*enc).gop_size = i32::MAX; // 무한 GOP: 온디맨드 IDR 만.
            (*enc).max_b_frames = 0;
            (*enc).hw_frames_ctx = av_buffer_ref(qsv_frames);

            let mut opts: *mut AVDictionary = ptr::null_mut();
            let set = |opts: &mut *mut AVDictionary, k: &str, v: &str| {
                let ck = CString::new(k).unwrap();
                let cv = CString::new(v).unwrap();
                unsafe { av_dict_set(opts as *mut _, ck.as_ptr(), cv.as_ptr(), 0) };
            };
            set(&mut opts, "preset", "veryfast");
            set(&mut opts, "low_delay_brc", "1");
            set(&mut opts, "forced_idr", "1");
            set(&mut opts, "async_depth", "1");
            set(&mut opts, "recovery_point_sei", "0");

            let ret = avcodec_open2(enc, codec, &mut opts);
            av_dict_free(&mut opts);
            if ret < 0 {
                bail!("avcodec_open2({codec_name}): {ret}");
            }

            let pkt = av_packet_alloc();
            if pkt.is_null() {
                bail!("alloc packet failed");
            }

            // 링 사전 매핑: 각 슬롯의 D3D11 hwframe 를 QSV 로 DIRECT 매핑을 한 번만 수행하고
            // 프레임을 살려둬(map 유지) 매 인코딩마다 재사용한다.
            let mut ring: Vec<MappedSlot> = Vec::with_capacity(RING_SIZE);
            for _ in 0..RING_SIZE {
                let d = av_frame_alloc();
                if d.is_null() {
                    bail!("alloc d3d11 ring frame failed");
                }
                let r = av_hwframe_get_buffer(d3d11_frames, d, 0);
                if r < 0 {
                    av_frame_free(&mut (d as *mut _) as *mut *mut AVFrame);
                    bail!("ring hwframe_get_buffer(d3d11): {r}");
                }
                let q = av_frame_alloc();
                if q.is_null() {
                    av_frame_free(&mut (d as *mut _) as *mut *mut AVFrame);
                    bail!("alloc qsv ring frame failed");
                }
                (*q).format = AV_PIX_FMT_QSV as c_int;
                (*q).hw_frames_ctx = av_buffer_ref(qsv_frames);
                (*q).width = out_w as c_int;
                (*q).height = out_h as c_int;
                let r = av_hwframe_map(q, d, AV_HWFRAME_MAP_DIRECT as c_int);
                if r < 0 {
                    av_frame_free(&mut (q as *mut _) as *mut *mut AVFrame);
                    av_frame_free(&mut (d as *mut _) as *mut *mut AVFrame);
                    bail!("ring hwframe_map(qsv): {r}");
                }
                ring.push(MappedSlot {
                    d3d11_frame: d,
                    qsv_frame: q,
                    tex: (*d).data[0] as *mut c_void,
                    index: (*d).data[1] as u32,
                });
            }

            tracing::info!(codec_name, out_w, out_h, fps, "zero-copy GPU encoder ready (d3d11va→qsv DIRECT)");
            Ok(Self {
                enc,
                d3d11_dev,
                qsv_dev,
                d3d11_frames,
                qsv_frames,
                ring,
                ring_pos: 0,
                staged: 0,
                ctx: ctx.clone(),
                _device: device.clone(),
                pkt,
                pts: 0,
                width: out_w,
                height: out_h,
            })
        }
    }

    /// NV12 텍스처를 다음 링 슬롯에 복사만 한다(빠른 GPU 복사). 이후 submit 으로 인코딩.
    /// 크로스-디바이스 경로: 공유 텍스처 반납 전 이 복사만 하고 keyed mutex 를 빨리 놓기 위함.
    pub fn stage(&mut self, nv12_tex: &ID3D11Texture2D) -> Result<()> {
        unsafe {
            let idx = self.ring_pos;
            self.ring_pos = (self.ring_pos + 1) % self.ring.len();
            self.staged = idx;
            let tex_ptr = self.ring[idx].tex;
            let sub_index = self.ring[idx].index;
            let dst_tex = ID3D11Texture2D::from_raw_borrowed(&tex_ptr)
                .ok_or_else(|| anyhow!("null ring texture"))?;
            self.ctx.CopySubresourceRegion(dst_tex, sub_index, 0, 0, 0, nv12_tex, 0, None);
            Ok(())
        }
    }

    /// 직전 stage 한 슬롯의 (이미 매핑된) QSV 프레임을 mfx 에 제출하고 패킷을 뽑는다(느린 부분).
    pub fn submit(&mut self, force_idr: bool) -> Result<Vec<EncodedPacket>> {
        unsafe {
            let qf = self.ring[self.staged].qsv_frame;
            (*qf).pts = self.pts;
            self.pts += 1;
            (*qf).pict_type = if force_idr { AV_PICTURE_TYPE_I } else { AV_PICTURE_TYPE_NONE };

            let sret = avcodec_send_frame(self.enc, qf);
            if sret < 0 {
                bail!("send_frame: {sret}");
            }

            let mut out = Vec::new();
            loop {
                let rret = avcodec_receive_packet(self.enc, self.pkt);
                if rret == AVERROR(EAGAIN) || rret == AVERROR_EOF {
                    break;
                }
                if rret < 0 {
                    bail!("receive_packet: {rret}");
                }
                let data = std::slice::from_raw_parts((*self.pkt).data, (*self.pkt).size as usize);
                let is_key = ((*self.pkt).flags & AV_PKT_FLAG_KEY as c_int) != 0;
                out.push(EncodedPacket { data: data.to_vec(), is_key_frame: is_key });
                av_packet_unref(self.pkt);
            }
            Ok(out)
        }
    }

    /// stage + submit (단독 테스트 및 하위호환 경로).
    pub fn encode(&mut self, nv12_tex: &ID3D11Texture2D, force_idr: bool) -> Result<Vec<EncodedPacket>> {
        self.stage(nv12_tex)?;
        self.submit(force_idr)
    }
}

impl Drop for ZeroCopyEncoder {
    fn drop(&mut self) {
        unsafe {
            // 링 프레임 먼저 해제(frames ctx 참조 → 컨텍스트보다 먼저).
            for slot in self.ring.drain(..) {
                let mut q = slot.qsv_frame;
                if !q.is_null() {
                    av_frame_free(&mut q as *mut *mut AVFrame);
                }
                let mut d = slot.d3d11_frame;
                if !d.is_null() {
                    av_frame_free(&mut d as *mut *mut AVFrame);
                }
            }
            if !self.pkt.is_null() {
                av_packet_free(&mut self.pkt);
            }
            if !self.enc.is_null() {
                avcodec_free_context(&mut self.enc);
            }
            if !self.qsv_frames.is_null() {
                av_buffer_unref(&mut self.qsv_frames);
            }
            if !self.d3d11_frames.is_null() {
                av_buffer_unref(&mut self.d3d11_frames);
            }
            if !self.qsv_dev.is_null() {
                av_buffer_unref(&mut self.qsv_dev);
            }
            if !self.d3d11_dev.is_null() {
                av_buffer_unref(&mut self.d3d11_dev);
            }
        }
    }
}

// 단일 캡처 스레드에서만 사용(COM, D3D11 컨텍스트 단일 스레드).
unsafe impl Send for ZeroCopyEncoder {}
