//! FFmpeg h264_qsv 하드웨어 인코더 래퍼 (Intel QuickSync).
//!
//! NV12 프레임 입력 → Annex-B H.264 패킷 출력. 저지연 설정(무한 GOP + 온디맨드 IDR,
//! B프레임 없음, low_delay_brc). Sunshine의 quicksync encoder_t 설정을 참조.
//!
//! R3d: 소프트웨어 MFT를 대체. GPU 색변환(VideoProcessor)에서 나온 NV12를 인코딩.

extern crate ffmpeg_next as ffmpeg;

use anyhow::{anyhow, bail, Result};
use ffmpeg::{codec, encoder, format, frame, Dictionary, Packet, Rational};

/// 인코딩된 H.264 프레임 (Annex-B).
pub struct EncodedPacket {
    pub data: Vec<u8>,
    pub is_key_frame: bool,
}

pub struct QsvEncoder {
    encoder: encoder::video::Encoder,
    codec_name: String,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    pts: i64,
    force_idr: bool,
}

impl QsvEncoder {
    /// h264_qsv 인코더 생성(하위호환 유지).
    pub fn new(width: u32, height: u32, fps: u32, bitrate_bps: u32) -> Result<Self> {
        Self::new_codec("h264_qsv", width, height, fps, bitrate_bps)
    }

    /// QSV 하드웨어 인코더 사용 가능 여부 프로브(작은 인코더를 실제로 열어봄).
    /// Intel GPU/드라이버(oneVPL/MediaSDK 런타임) 부재·구버전이면 false.
    pub fn probe_available() -> bool {
        Self::new_codec("h264_qsv", 128, 128, 30, 1_000_000).is_ok()
    }

    /// 지정 QSV 인코더(`h264_qsv` 또는 `hevc_qsv`) 생성. 저지연 구성.
    /// HEVC 는 동일 대역폭에 H.264 보다 30~50% 선명(고해상도 이득 큼).
    pub fn new_codec(codec_name: &str, width: u32, height: u32, fps: u32, bitrate_bps: u32) -> Result<Self> {
        // 전역 1회 초기화(중복 무해).
        ffmpeg::init().map_err(|e| anyhow!("ffmpeg init: {e}"))?;
        let encoder = Self::build_encoder(codec_name, width, height, fps, bitrate_bps)?;
        Ok(Self {
            encoder,
            codec_name: codec_name.to_string(),
            width,
            height,
            fps,
            bitrate: bitrate_bps,
            pts: 0,
            force_idr: false,
        })
    }

    /// 저지연 QSV 인코더 하나를 열어 반환한다. new_codec 최초 생성과 set_bitrate 재생성이 공유.
    fn build_encoder(
        codec_name: &str,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u32,
    ) -> Result<encoder::video::Encoder> {
        let codec = encoder::find_by_name(codec_name)
            .ok_or_else(|| anyhow!("{codec_name} not found"))?;
        let ctx = codec::context::Context::new_with_codec(codec);
        let mut video = ctx.encoder().video().map_err(|e| anyhow!("encoder video ctx: {e}"))?;

        video.set_width(width);
        video.set_height(height);
        video.set_format(format::Pixel::NV12);
        video.set_time_base(Rational(1, fps as i32));
        video.set_frame_rate(Some(Rational(fps as i32, 1)));
        video.set_bit_rate(bitrate_bps as usize);
        video.set_max_bit_rate(bitrate_bps as usize);
        // 무한 GOP: 키프레임은 온디맨드로만 (Moonlight RequestIdrFrame).
        video.set_gop(i32::MAX as u32);
        video.set_max_b_frames(0);

        let mut opts = Dictionary::new();
        opts.set("preset", "veryfast");
        opts.set("forced_idr", "1");      // pict_type=I 강제 시 진짜 IDR + SPS/PPS(HEVC=VPS/SPS/PPS).
        opts.set("low_delay_brc", "1");   // 저지연 레이트컨트롤.
        opts.set("async_depth", "1");     // 프레임 지연 최소화.
        opts.set("recovery_point_sei", "0");

        video.open_with(opts).map_err(|e| anyhow!("open {codec_name}: {e}"))
    }

    /// 목표 비트레이트로 인코더를 재생성한다(QSV 런타임 재구성 불안정 → 컨텍스트 재생성).
    /// 새 인코더 첫 프레임은 IDR 이라 매끄럽게 전환. 실패 시 기존 인코더 유지 + Err.
    pub fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<()> {
        if bitrate_bps == self.bitrate {
            return Ok(());
        }
        let new_enc = Self::build_encoder(&self.codec_name, self.width, self.height, self.fps, bitrate_bps)?;
        self.encoder = new_enc;
        self.bitrate = bitrate_bps;
        self.pts = 0;
        self.force_idr = true;
        tracing::info!(bitrate_bps, "RAM encoder bitrate reconfigured");
        Ok(())
    }

    /// 다음 프레임을 IDR(키프레임)로 강제.
    pub fn request_idr(&mut self) {
        self.force_idr = true;
    }

    /// NV12 평면 데이터(Y: width*height, UV: width*height/2, 타이트 팩)를 인코딩.
    /// 준비된 Annex-B 패킷들을 반환.
    pub fn encode(&mut self, nv12: &[u8]) -> Result<Vec<EncodedPacket>> {
        let (w, h) = (self.width as usize, self.height as usize);
        let expected = w * h * 3 / 2;
        if nv12.len() < expected {
            bail!("NV12 too small: {} < {}", nv12.len(), expected);
        }

        let mut vframe = frame::Video::new(format::Pixel::NV12, self.width, self.height);
        // Y 평면 (stride 고려 복사).
        {
            let stride = vframe.stride(0);
            let dst = vframe.data_mut(0);
            for y in 0..h {
                dst[y * stride..y * stride + w].copy_from_slice(&nv12[y * w..y * w + w]);
            }
        }
        // UV 평면 (h/2 행, width 바이트).
        {
            let uv_off = w * h;
            let stride = vframe.stride(1);
            let dst = vframe.data_mut(1);
            for y in 0..(h / 2) {
                dst[y * stride..y * stride + w]
                    .copy_from_slice(&nv12[uv_off + y * w..uv_off + y * w + w]);
            }
        }
        vframe.set_pts(Some(self.pts));
        if self.force_idr {
            vframe.set_kind(ffmpeg::picture::Type::I);
            self.force_idr = false;
        } else {
            vframe.set_kind(ffmpeg::picture::Type::None);
        }
        self.pts += 1;

        self.encoder.send_frame(&vframe).map_err(|e| anyhow!("send_frame: {e}"))?;
        self.drain()
    }

    fn drain(&mut self) -> Result<Vec<EncodedPacket>> {
        let mut out = Vec::new();
        let mut pkt = Packet::empty();
        while self.encoder.receive_packet(&mut pkt).is_ok() {
            if let Some(data) = pkt.data() {
                out.push(EncodedPacket { data: data.to_vec(), is_key_frame: pkt.is_key() });
            }
        }
        Ok(out)
    }
}
