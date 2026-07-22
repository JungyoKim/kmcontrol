//! Media Foundation H.264 인코더 (IMFTransform).
//!
//! R3b: 먼저 소프트웨어 인코더(CLSID_CMSH264EncoderMFT, sync)로 NAL→패킷화→Moonlight 경로를
//! 검증한다. 소프트웨어 인코더는 Annex-B(시작코드 포함) H.264 elementary stream을 출력하며
//! sync ProcessInput/ProcessOutput API라 배관이 단순하다. 이후 하드웨어 QSV(async MFT)로 전환.
//!
//! 입력: NV12 (BGRA→NV12 변환은 호출자/헬퍼). 출력: Annex-B H.264 NAL 바이트.
//!
//! 참조: Microsoft MF H.264 encoder MFT 문서. windows-rs 0.62.

use anyhow::{anyhow, bail, Result};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

/// MF 런타임 1회 초기화.
pub fn mf_startup() -> Result<()> {
    unsafe {
        MFStartup(MF_VERSION, MFSTARTUP_LITE).map_err(|e| anyhow!("MFStartup: {e}"))?;
    }
    Ok(())
}

/// FRAME_SIZE / FRAME_RATE 등 두 u32를 u64로 패킹.
fn pack_u64(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | (lo as u64)
}

/// 인코딩된 H.264 프레임 (Annex-B) + 키프레임 여부.
pub struct EncodedPacket {
    pub data: Vec<u8>,
    pub is_key_frame: bool,
}

pub struct H264Encoder {
    transform: IMFTransform,
    width: u32,
    height: u32,
    /// 출력 스트림 정보에서 얻은 버퍼 크기(호스트 할당 필요 시).
    output_provides_samples: bool,
    output_buf_size: u32,
}

// H264Encoder는 캡처 스레드에서 생성·사용되며 스레드 간 공유되지 않는다.
// windows-capture가 핸들러에 Send를 요구하므로 안전하게 Send를 단언한다.
unsafe impl Send for H264Encoder {}

impl H264Encoder {
    /// 소프트웨어 H.264 인코더 생성 + 타입 협상.
    pub fn new_software(width: u32, height: u32, fps: u32, bitrate_bps: u32) -> Result<Self> {
        unsafe {
            let transform: IMFTransform =
                CoCreateInstance(&CLSID_MSH264EncoderMFT, None, CLSCTX_INPROC_SERVER)
                    .map_err(|e| anyhow!("create H264 encoder MFT: {e}"))?;

            // 1. 출력 타입 (H.264) — 입력보다 먼저.
            let out_type: IMFMediaType = MFCreateMediaType().map_err(|e| anyhow!("create out type: {e}"))?;
            out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            out_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_bps)?;
            out_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(width, height))?;
            out_type.SetUINT64(&MF_MT_FRAME_RATE, pack_u64(fps, 1))?;
            out_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u64(1, 1))?;
            out_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            // Main 프로파일.
            out_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)?;
            transform
                .SetOutputType(0, &out_type, 0)
                .map_err(|e| anyhow!("set output type: {e}"))?;

            // 2. 입력 타입 (NV12).
            let in_type: IMFMediaType = MFCreateMediaType().map_err(|e| anyhow!("create in type: {e}"))?;
            in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            in_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            in_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(width, height))?;
            in_type.SetUINT64(&MF_MT_FRAME_RATE, pack_u64(fps, 1))?;
            in_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u64(1, 1))?;
            in_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            transform
                .SetInputType(0, &in_type, 0)
                .map_err(|e| anyhow!("set input type: {e}"))?;

            // 출력 스트림 정보: 샘플을 MFT가 제공하는지, 버퍼 크기.
            let out_info = transform.GetOutputStreamInfo(0).map_err(|e| anyhow!("output stream info: {e}"))?;
            let provides = (out_info.dwFlags
                & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                    | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32))
                != 0;

            // 스트리밍 시작.
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

            Ok(Self {
                transform,
                width,
                height,
                output_provides_samples: provides,
                output_buf_size: out_info.cbSize.max((width * height * 3 / 2).max(1)),
            })
        }
    }

    /// NV12 프레임 하나를 인코딩. 준비된 출력 패킷들을 반환(0개 이상).
    /// `time_100ns`/`dur_100ns`는 100ns 단위 타임스탬프/지속시간.
    pub fn encode_nv12(&mut self, nv12: &[u8], time_100ns: i64, dur_100ns: i64) -> Result<Vec<EncodedPacket>> {
        let expected = (self.width * self.height * 3 / 2) as usize;
        if nv12.len() < expected {
            bail!("NV12 buffer too small: {} < {}", nv12.len(), expected);
        }
        unsafe {
            // 입력 샘플 구성.
            let buffer = MFCreateMemoryBuffer(nv12.len() as u32).map_err(|e| anyhow!("create input buffer: {e}"))?;
            {
                let mut ptr: *mut u8 = std::ptr::null_mut();
                let mut max_len = 0u32;
                buffer.Lock(&mut ptr, Some(&mut max_len), None)?;
                std::ptr::copy_nonoverlapping(nv12.as_ptr(), ptr, nv12.len());
                buffer.SetCurrentLength(nv12.len() as u32)?;
                buffer.Unlock()?;
            }
            let sample = MFCreateSample().map_err(|e| anyhow!("create input sample: {e}"))?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(time_100ns)?;
            sample.SetSampleDuration(dur_100ns)?;

            self.transform
                .ProcessInput(0, &sample, 0)
                .map_err(|e| anyhow!("process input: {e}"))?;

            self.drain_output()
        }
    }

    /// 대기 중인 출력 샘플을 모두 뽑아낸다.
    unsafe fn drain_output(&mut self) -> Result<Vec<EncodedPacket>> {
        let mut packets = Vec::new();
        loop {
            // 출력 버퍼 준비 (MFT가 샘플을 제공하지 않으면 우리가 할당).
            let mut out_sample: Option<IMFSample> = None;
            if !self.output_provides_samples {
                let s = MFCreateSample().map_err(|e| anyhow!("create out sample: {e}"))?;
                let b = MFCreateMemoryBuffer(self.output_buf_size).map_err(|e| anyhow!("create out buffer: {e}"))?;
                s.AddBuffer(&b)?;
                out_sample = Some(s);
            }

            let mut out_buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: std::mem::ManuallyDrop::new(out_sample.clone()),
                dwStatus: 0,
                pEvents: std::mem::ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let hr = self.transform.ProcessOutput(0, &mut out_buffers, &mut status);

            match hr {
                Ok(()) => {
                    let produced = std::mem::ManuallyDrop::take(&mut out_buffers[0].pSample);
                    if let Some(sample) = produced.or(out_sample) {
                        if let Some(pkt) = Self::sample_to_packet(&sample)? {
                            packets.push(pkt);
                        }
                    }
                }
                Err(e) => {
                    // MF_E_TRANSFORM_NEED_MORE_INPUT: 더 이상 출력 없음.
                    if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
                        break;
                    }
                    // MF_E_TRANSFORM_STREAM_CHANGE: 출력 타입 재협상 필요(간단 처리: 무시하고 계속).
                    if e.code() == MF_E_TRANSFORM_STREAM_CHANGE {
                        break;
                    }
                    return Err(anyhow!("process output: {e}"));
                }
            }
        }
        Ok(packets)
    }

    /// IMFSample → Annex-B 바이트 + 키프레임 판정.
    unsafe fn sample_to_packet(sample: &IMFSample) -> Result<Option<EncodedPacket>> {
        let buffer = sample.ConvertToContiguousBuffer().map_err(|e| anyhow!("contiguous buffer: {e}"))?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut cur_len = 0u32;
        buffer.Lock(&mut ptr, None, Some(&mut cur_len))?;
        let data = std::slice::from_raw_parts(ptr, cur_len as usize).to_vec();
        buffer.Unlock()?;
        if data.is_empty() {
            return Ok(None);
        }
        // 키프레임: MFSampleExtension_CleanPoint 속성.
        let is_key_frame = sample
            .GetUINT32(&MFSampleExtension_CleanPoint)
            .map(|v| v != 0)
            .unwrap_or(false);
        Ok(Some(EncodedPacket { data, is_key_frame }))
    }

    /// 키프레임(IDR) 강제 요청.
    pub fn request_key_frame(&self) -> Result<()> {
        // MFT에 다음 프레임을 IDR로 만들도록 요청하는 표준 경로는 코덱 API 속성이지만,
        // 소프트웨어 인코더는 GOP 주기로 자동 IDR을 낸다. R3b에서는 no-op.
        Ok(())
    }
}

/// BGRA (top-down, stride=width*4) → NV12 (BT.601 limited range) CPU 변환.
/// 실시간엔 GPU 변환이 낫지만 R3b 검증용으로 충분.
pub fn bgra_to_nv12(bgra: &[u8], width: usize, height: usize, out: &mut Vec<u8>) {
    let y_size = width * height;
    let uv_size = y_size / 2;
    out.clear();
    out.resize(y_size + uv_size, 0);
    let (y_plane, uv_plane) = out.split_at_mut(y_size);

    for j in 0..height {
        for i in 0..width {
            let idx = (j * width + i) * 4;
            let b = bgra[idx] as f32;
            let g = bgra[idx + 1] as f32;
            let r = bgra[idx + 2] as f32;
            // BT.601 limited.
            let y = (0.257 * r + 0.504 * g + 0.098 * b + 16.0).round().clamp(0.0, 255.0);
            y_plane[j * width + i] = y as u8;
        }
    }
    // 2x2 서브샘플 색차.
    for j in (0..height).step_by(2) {
        for i in (0..width).step_by(2) {
            let idx = (j * width + i) * 4;
            let b = bgra[idx] as f32;
            let g = bgra[idx + 1] as f32;
            let r = bgra[idx + 2] as f32;
            let u = (-0.148 * r - 0.291 * g + 0.439 * b + 128.0).round().clamp(0.0, 255.0);
            let v = (0.439 * r - 0.368 * g - 0.071 * b + 128.0).round().clamp(0.0, 255.0);
            let uv_row = j / 2;
            let uv_col = i / 2;
            let off = uv_row * width + uv_col * 2;
            uv_plane[off] = u as u8;
            uv_plane[off + 1] = v as u8;
        }
    }
}

/// BGRA(src_w×src_h) → NV12(tw×th) 융합: nearest-neighbor 다운스케일 + BT.601 색변환을
/// 한 번에, rayon으로 행 병렬 처리. 정수 연산(부동소수점 제거)으로 고속화.
/// `out`은 tw*th*3/2 크기로 리사이즈됨. tw/th는 짝수 가정.
pub fn bgra_scale_to_nv12(
    bgra: &[u8],
    src_w: usize,
    src_h: usize,
    tw: usize,
    th: usize,
    out: &mut Vec<u8>,
) {
    use rayon::prelude::*;

    let y_size = tw * th;
    let uv_size = y_size / 2;
    out.clear();
    out.resize(y_size + uv_size, 0);
    let (y_plane, uv_plane) = out.split_at_mut(y_size);

    // Y 평면: 출력 행별 병렬.
    y_plane
        .par_chunks_mut(tw)
        .enumerate()
        .for_each(|(y, row)| {
            let sy = y * src_h / th;
            let src_row = sy * src_w * 4;
            for (x, py) in row.iter_mut().enumerate() {
                let sx = x * src_w / tw;
                let idx = src_row + sx * 4;
                let b = bgra[idx] as u32;
                let g = bgra[idx + 1] as u32;
                let r = bgra[idx + 2] as u32;
                // BT.601 limited, 정수 근사 (<<8 고정소수점).
                let yv = (66 * r + 129 * g + 25 * b + 128) >> 8;
                *py = (yv + 16) as u8;
            }
        });

    // UV 평면: 2x2 서브샘플, 출력 UV 행별 병렬.
    uv_plane
        .par_chunks_mut(tw)
        .enumerate()
        .for_each(|(uv_row, row)| {
            let y = uv_row * 2;
            let sy = y * src_h / th;
            let src_row = sy * src_w * 4;
            for uv_col in 0..(tw / 2) {
                let x = uv_col * 2;
                let sx = x * src_w / tw;
                let idx = src_row + sx * 4;
                let b = bgra[idx] as i32;
                let g = bgra[idx + 1] as i32;
                let r = bgra[idx + 2] as i32;
                let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
                row[uv_col * 2] = u.clamp(0, 255) as u8;
                row[uv_col * 2 + 1] = v.clamp(0, 255) as u8;
            }
        });
}
