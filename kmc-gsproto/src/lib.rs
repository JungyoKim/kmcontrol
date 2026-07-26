//! GameStream 비디오 와이어 포맷 — 호스트와 클라이언트가 공유하는 **단일 소스**.
//!
//! shard 레이아웃 (moonlight-common-c / Sunshine / moonshine 참조):
//!   `[ RTP 헤더(12) | 패딩(4) | NvVideoPacket(16) | payload(= packet_size - 16) ]`
//! payload 스트림 = `[ VideoFrameHeader(8) ++ encoded_NAL ]` 을 shard 크기로 분할한 것.
//! `VideoFrameHeader` 는 논리 오프셋 0..8 이므로 **첫 데이터 shard 에만** 실린다.
//!
//! 바이트 오더 주의: RTP 헤더는 **big-endian**, NvVideoPacket/VideoFrameHeader 는 **little-endian**.
//!
//! `kmc-streamhost`(패킷화)와 `kmc-moonclient`(디패킷화 + FEC 복구)가 같은 정의를 쓴다.
//! 한쪽만 고치면 런타임에서만 드러나는 스펙 불일치가 되므로 정의를 여기 모은다.
//! 의존성 없음(순수 std) — 양쪽 크레이트 어디에도 빌드 부담을 주지 않는다.

#![forbid(unsafe_code)]

/// NvVideoPacket 헤더 크기.
pub const NV_VIDEO_PACKET_SIZE: usize = 16;
/// RTP 헤더 크기.
pub const RTP_HEADER_SIZE: usize = 12;
/// RTP 헤더와 NvVideoPacket 사이의 패딩.
pub const PADDING_SIZE: usize = 4;
/// shard 시작부터 NvVideoPacket 까지의 오프셋 (12 + 4 = 16).
pub const NV_PACKET_OFFSET: usize = RTP_HEADER_SIZE + PADDING_SIZE;
/// shard 시작부터 payload 까지의 오프셋 (16 + 16 = 32).
pub const PAYLOAD_OFFSET: usize = NV_PACKET_OFFSET + NV_VIDEO_PACKET_SIZE;
/// VideoFrameHeader 크기.
pub const VIDEO_FRAME_HEADER_SIZE: usize = 8;

/// RTP 헤더 첫 바이트(version/flags). Sunshine 고정값.
pub const RTP_VERSION_FLAGS: u8 = 0x90;
/// NvVideoPacket `multi_fec_flags` 고정값.
pub const MULTI_FEC_FLAGS: u8 = 0x10;
/// FEC 블록 최대 개수 — `multi_fec_blocks` 의 블록 인덱스 필드가 2비트라서 4.
pub const MAX_FEC_BLOCKS: usize = 4;
/// 한 FEC 블록의 최대 shard 수 — Reed-Solomon GF(256) 상한.
pub const MAX_SHARDS: usize = 255;

/// `VideoFrameHeader.frame_type` 값.
pub const FRAME_TYPE_DELTA: u8 = 1;
/// `VideoFrameHeader.frame_type` 값 (IDR).
pub const FRAME_TYPE_KEY: u8 = 2;
/// `VideoFrameHeader.header_type` 고정값.
pub const FRAME_HEADER_TYPE: u8 = 0x01;

/// NvVideoPacket `flags` 비트.
pub mod rtp_flag {
    /// 이 shard 가 픽처 데이터를 담고 있다(= 데이터 shard, parity 아님).
    pub const CONTAINS_PIC_DATA: u8 = 0x1;
    /// 프레임의 마지막 데이터 shard.
    pub const END_OF_FRAME: u8 = 0x2;
    /// 프레임의 첫 데이터 shard.
    pub const START_OF_FRAME: u8 = 0x4;
}

/// GameStream 기본 베이스 포트(HTTP). 나머지 포트는 여기서 고정 오프셋으로 파생한다 —
/// Sunshine 의 `port` 설정과 같은 규약이라 Moonlight 클라이언트가 그대로 붙는다.
pub const DEFAULT_BASE_PORT: u16 = 47989;

/// 베이스 포트 → `(http, https, rtsp)`. 오프셋: https = base-5, rtsp = base+21.
/// 미디어 포트(video/control/audio)는 RTSP SETUP 으로 협상하므로 여기 없다.
#[inline]
pub fn ports_from_base(base: u16) -> (u16, u16, u16) {
    (base, base.wrapping_sub(5), base.wrapping_add(21))
}

/// 데이터그램 길이에서 shard payload 길이를 역산한다.
///
/// 송신 측은 `shard_payload_len = packet_size - NV_VIDEO_PACKET_SIZE` 로 잡으므로
/// 전체 데이터그램은 `PAYLOAD_OFFSET + shard_payload_len` 바이트다.
/// 헤더보다 짧은(=손상/무관) 데이터그램이면 `None`.
#[inline]
pub fn shard_payload_len(datagram_len: usize) -> Option<usize> {
    datagram_len.checked_sub(PAYLOAD_OFFSET).filter(|n| *n > 0)
}

/// 한 FEC 블록의 parity shard 개수 — **송신/수신 양쪽이 반드시 이 함수를 쓴다**.
///
/// Reed-Solomon 생성행렬의 parity 행은 총 parity 개수에 의존한다(fec-rs `build_matrix(rows, cols)`).
/// 즉 `RS(13,3)` 이 만든 parity 를 `RS(13,2)` 로 복구하면 **에러 없이 쓰레기**가 나온다. 그래서
/// 수신 측이 이 값을 정확히 재현하지 못하면 FEC 가 조용히 프레임을 망가뜨린다. 와이어에는
/// `fec_percentage` 만 실리므로, 그 하나에서 양쪽이 같은 식으로 개수를 뽑아야 한다.
///
/// 식은 moonlight-common-c `RtpVideoQueue.c` 의 `(dataShards * fecPercentage + 99) / 100`
/// (= 올림)과 동일하다 — 서드파티 Moonlight 클라이언트와도 그대로 맞물린다.
/// `MAX_SHARDS` 상한 때문에 `data + parity` 가 넘칠 때는 잘라낸다(양쪽 동일).
///
/// `fec_percentage == 0` 은 **parity 없음** 모드다 - C 레퍼런스도 0 을 낸다. 여기서 1 로 올려버리면
/// parity 없는 프레임마다 클라이언트가 있지도 않은 shard 를 기다리며 시퀀스 창이 어긋난다.
/// 반대로 퍼센트가 0 이 아니면 절대 0 으로 내리지 않는다(요청한 보호를 무보호로 만들지 않는다).
#[inline]
pub fn parity_shard_count(data_shards: usize, fec_percentage: usize) -> usize {
    if data_shards == 0 || fec_percentage == 0 {
        return 0;
    }
    (data_shards * fec_percentage)
        .div_ceil(100)
        .max(1)
        .min(MAX_SHARDS.saturating_sub(data_shards))
}

/// RTP 헤더 (big-endian). `version_flags`/`packet_type` 은 우리 프로토콜에서 상수라 필드로 두지 않는다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct RtpHeader {
    pub sequence_number: u16,
    pub timestamp: u32,
    pub ssrc: u32,
}

impl RtpHeader {
    /// shard 시작에 기록한다. `buf` 는 최소 [`RTP_HEADER_SIZE`] 바이트여야 한다.
    pub fn write(&self, buf: &mut [u8]) {
        let buf = &mut buf[..RTP_HEADER_SIZE];
        buf[0] = RTP_VERSION_FLAGS;
        buf[1] = 0; // packet_type
        buf[2..4].copy_from_slice(&self.sequence_number.to_be_bytes());
        buf[4..8].copy_from_slice(&self.timestamp.to_be_bytes());
        buf[8..12].copy_from_slice(&self.ssrc.to_be_bytes());
    }

    /// shard 앞부분을 파싱한다. 길이가 모자라면 `None`.
    pub fn read(buf: &[u8]) -> Option<Self> {
        let buf = buf.get(..RTP_HEADER_SIZE)?;
        Some(Self {
            sequence_number: u16::from_be_bytes([buf[2], buf[3]]),
            timestamp: u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
            ssrc: u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]),
        })
    }
}

/// NvVideoPacket 헤더 (little-endian).
///
/// `stream_packet_index` 는 **논리값**이다 — 와이어에는 상위 24비트에 실리므로
/// [`Self::write`] 가 `<< 8`, [`Self::read`] 가 `>> 8` 을 적용한다.
/// 데이터 패킷만 증가하는 전역 카운터로, Moonlight 디패킷타이저가 연속성을 요구한다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct NvVideoPacket {
    pub stream_packet_index: u32,
    pub frame_index: u32,
    pub flags: u8,
    pub multi_fec_blocks: u8,
    pub fec_info: u32,
}

impl NvVideoPacket {
    /// `buf` 는 최소 [`NV_VIDEO_PACKET_SIZE`] 바이트여야 한다.
    pub fn write(&self, buf: &mut [u8]) {
        let buf = &mut buf[..NV_VIDEO_PACKET_SIZE];
        buf[0..4].copy_from_slice(&(self.stream_packet_index << 8).to_le_bytes());
        buf[4..8].copy_from_slice(&self.frame_index.to_le_bytes());
        buf[8] = self.flags;
        buf[9] = 0; // reserved
        buf[10] = MULTI_FEC_FLAGS;
        buf[11] = self.multi_fec_blocks;
        buf[12..16].copy_from_slice(&self.fec_info.to_le_bytes());
    }

    /// shard 의 [`NV_PACKET_OFFSET`] 이후 구간을 파싱한다.
    pub fn read(buf: &[u8]) -> Option<Self> {
        let buf = buf.get(..NV_VIDEO_PACKET_SIZE)?;
        Some(Self {
            stream_packet_index: u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) >> 8,
            frame_index: u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
            flags: buf[8],
            multi_fec_blocks: buf[11],
            fec_info: u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]),
        })
    }

    /// 전체 shard 데이터그램에서 NvVideoPacket 구간만 파싱한다.
    pub fn read_from_shard(shard: &[u8]) -> Option<Self> {
        Self::read(shard.get(NV_PACKET_OFFSET..)?)
    }

    /// 데이터 shard 인가(parity 가 아닌가).
    #[inline]
    pub fn contains_pic_data(&self) -> bool {
        self.flags & rtp_flag::CONTAINS_PIC_DATA != 0
    }

    /// 프레임의 첫 데이터 shard 인가.
    #[inline]
    pub fn start_of_frame(&self) -> bool {
        self.flags & rtp_flag::START_OF_FRAME != 0
    }

    /// 프레임의 마지막 데이터 shard 인가.
    #[inline]
    pub fn end_of_frame(&self) -> bool {
        self.flags & rtp_flag::END_OF_FRAME != 0
    }

    /// 이 shard 가 속한 FEC 블록 인덱스 (`multi_fec_blocks` 비트 4-5).
    #[inline]
    pub fn block_index(&self) -> usize {
        ((self.multi_fec_blocks >> 4) & 0x3) as usize
    }

    /// 이 프레임의 FEC 블록 개수 (`multi_fec_blocks` 비트 6-7 에 `blocks-1` 이 실린다).
    #[inline]
    pub fn block_count(&self) -> usize {
        (((self.multi_fec_blocks >> 6) & 0x3) + 1) as usize
    }

    /// 블록 내 shard 인덱스 (`fec_info` 비트 12-21). 데이터 shard 는 `0..data_shards`,
    /// parity shard 는 `data_shards..data_shards+parity`.
    #[inline]
    pub fn shard_index(&self) -> usize {
        ((self.fec_info >> 12) & 0x3FF) as usize
    }

    /// 이 블록의 데이터 shard 개수 (`fec_info` 비트 22-31).
    #[inline]
    pub fn data_shards(&self) -> usize {
        ((self.fec_info >> 22) & 0x3FF) as usize
    }

    /// 이 블록에 실제 적용된 parity 비율 % (`fec_info` 비트 4-11).
    #[inline]
    pub fn fec_percentage(&self) -> usize {
        ((self.fec_info >> 4) & 0xFF) as usize
    }

    /// 이 블록의 parity shard 개수. 송신 측이 쓴 [`parity_shard_count`] 와 **같은 함수**로 뽑는다.
    #[inline]
    pub fn parity_shards(&self) -> usize {
        parity_shard_count(self.data_shards(), self.fec_percentage())
    }

    /// `fec_info` 필드를 조립한다 (송신 측용).
    #[inline]
    pub fn pack_fec_info(shard_index: usize, data_shards: usize, fec_percentage: usize) -> u32 {
        (shard_index << 12 | data_shards << 22 | fec_percentage << 4) as u32
    }

    /// `multi_fec_blocks` 필드를 조립한다 (송신 측용).
    #[inline]
    pub fn pack_multi_fec_blocks(block_index: usize, block_count: usize) -> u8 {
        ((block_index as u8) << 4) | ((block_count as u8 - 1) << 6)
    }
}

/// VideoFrameHeader (8바이트, little-endian). payload 스트림의 맨 앞에 한 번만 실린다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct VideoFrameHeader {
    pub is_key_frame: bool,
    /// 호스트 인코드 지연(ms 단위 아님 — Sunshine 확장). 우리 호스트는 0 을 쓴다.
    pub frame_processing_latency: u16,
    /// **마지막** 데이터 shard 의 유효 payload 길이. 수신 측이 마지막 shard 를 트림할 때 쓴다.
    pub last_payload_len: u32,
}

impl VideoFrameHeader {
    pub fn write(&self, buf: &mut [u8; VIDEO_FRAME_HEADER_SIZE]) {
        buf[0] = FRAME_HEADER_TYPE;
        buf[1..3].copy_from_slice(&self.frame_processing_latency.to_le_bytes());
        buf[3] = if self.is_key_frame { FRAME_TYPE_KEY } else { FRAME_TYPE_DELTA };
        buf[4..8].copy_from_slice(&self.last_payload_len.to_le_bytes());
    }

    pub fn read(buf: &[u8]) -> Option<Self> {
        let buf = buf.get(..VIDEO_FRAME_HEADER_SIZE)?;
        Some(Self {
            is_key_frame: buf[3] == FRAME_TYPE_KEY,
            frame_processing_latency: u16::from_le_bytes([buf[1], buf[2]]),
            last_payload_len: u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
        })
    }
}

/// control 채널 메시지 타입: Sunshine 확장 `SS_FRAME_FEC_STATUS`.
/// 헤더는 little-endian(type/length)이지만 **본문은 big-endian** 이다.
pub const MSG_FRAME_FEC_STATUS: u16 = 0x5502;

/// `SS_FRAME_FEC_STATUS` 본문 (21바이트, **big-endian**).
///
/// 클라이언트 디패킷타이저가 한 프레임을 마감할 때 채워서 control 채널로 보내고,
/// 호스트 비트레이트 컨트롤러가 손실률 신호로 소비한다. FEC 로 복구된 프레임이어도
/// 손실이 있었으면 보고한다 — 네트워크 열화의 조기 지표이기 때문이다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct FrameFecStatus {
    pub frame_index: u32,
    pub highest_received_seq: u16,
    pub next_contiguous_seq: u16,
    pub missing_before_highest: u16,
    pub total_data_shards: u16,
    pub total_parity_shards: u16,
    pub received_data_shards: u16,
    pub received_parity_shards: u16,
    pub fec_percentage: u8,
    pub block_index: u8,
    pub block_count: u8,
}

impl FrameFecStatus {
    /// 직렬화 크기.
    pub const SIZE: usize = 21;

    /// big-endian 으로 기록한다. `buf.len() < SIZE` 면 `false`.
    pub fn write(&self, buf: &mut [u8]) -> bool {
        if buf.len() < Self::SIZE {
            return false;
        }
        buf[0..4].copy_from_slice(&self.frame_index.to_be_bytes());
        buf[4..6].copy_from_slice(&self.highest_received_seq.to_be_bytes());
        buf[6..8].copy_from_slice(&self.next_contiguous_seq.to_be_bytes());
        buf[8..10].copy_from_slice(&self.missing_before_highest.to_be_bytes());
        buf[10..12].copy_from_slice(&self.total_data_shards.to_be_bytes());
        buf[12..14].copy_from_slice(&self.total_parity_shards.to_be_bytes());
        buf[14..16].copy_from_slice(&self.received_data_shards.to_be_bytes());
        buf[16..18].copy_from_slice(&self.received_parity_shards.to_be_bytes());
        buf[18] = self.fec_percentage;
        buf[19] = self.block_index;
        buf[20] = self.block_count;
        true
    }

    /// 21바이트 배열로 직렬화.
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        self.write(&mut buf);
        buf
    }

    /// big-endian 본문을 파싱한다. 21바이트 미만이면 `None`.
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < Self::SIZE {
            return None;
        }
        let be16 = |o: usize| u16::from_be_bytes([buf[o], buf[o + 1]]);
        Some(Self {
            frame_index: u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]),
            highest_received_seq: be16(4),
            next_contiguous_seq: be16(6),
            missing_before_highest: be16(8),
            total_data_shards: be16(10),
            total_parity_shards: be16(12),
            received_data_shards: be16(14),
            received_parity_shards: be16(16),
            fec_percentage: buf[18],
            block_index: buf[19],
            block_count: buf[20],
        })
    }

    /// 이 프레임의 패킷 손실 비율(0.0~1.0). 전송 패킷이 0이면 `None`.
    /// 수신 > 전송(비정상 보고)이면 0.0 으로 클램프한다.
    pub fn loss_fraction(&self) -> Option<f32> {
        let total = self.total_data_shards as u32 + self.total_parity_shards as u32;
        if total == 0 {
            return None;
        }
        let recv =
            (self.received_data_shards as u32 + self.received_parity_shards as u32).min(total);
        Some((total - recv) as f32 / total as f32)
    }
}

/// 비디오/오디오 UDP 소켓의 클라이언트 등록 겸 keepalive 핑.
///
/// 호스트는 이 4바이트를 받은 소스 주소를 스트림 목적지로 등록하고, 3초간 끊기면 인코딩을
/// 멈춘다(`kmc-streamhost/src/video/mod.rs`). 클라이언트 쪽 NAT 는 이 패킷으로 스스로 뚫린다.
pub const PING_PAYLOAD: &[u8; 4] = b"PING";

/// RTT 프로브 태그. 호스트가 PING 에 대한 답으로 `PONG_TAG + u64(호스트 단조시계 마이크로초)`
/// 를 되쏘고, 클라이언트는 받은 12바이트를 **그대로** 되돌려보낸다.
///
/// 타임스탬프를 찍는 쪽과 RTT 를 계산하는 쪽이 모두 호스트이므로 **시계 동기화가 필요 없다**.
/// 클라이언트는 상태를 전혀 갖지 않는다(순수 에코).
pub const PONG_TAG: &[u8; 4] = b"PONG";

/// RTT 프로브 데이터그램 크기(태그 4 + u64 타임스탬프 8).
pub const RTT_PROBE_SIZE: usize = 12;

/// RTT 프로브 데이터그램을 만든다. `micros` 는 호스트의 단조시계 기준 경과 마이크로초.
pub fn rtt_probe(micros: u64) -> [u8; RTT_PROBE_SIZE] {
    let mut buf = [0u8; RTT_PROBE_SIZE];
    buf[..4].copy_from_slice(PONG_TAG);
    buf[4..].copy_from_slice(&micros.to_le_bytes());
    buf
}

/// 되돌아온 RTT 프로브에서 타임스탬프를 꺼낸다. 프로브가 아니면 `None`.
///
/// 실제 미디어 shard 는 최소 [`PAYLOAD_OFFSET`] 바이트라 12바이트 프로브와 절대 겹치지 않는다.
pub fn parse_rtt_probe(buf: &[u8]) -> Option<u64> {
    if buf.len() != RTT_PROBE_SIZE || &buf[..4] != PONG_TAG {
        return None;
    }
    Some(u64::from_le_bytes(buf[4..].try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtp_header_golden_bytes() {
        let mut buf = [0u8; RTP_HEADER_SIZE];
        RtpHeader { sequence_number: 0x1234, timestamp: 0xDEADBEEF, ssrc: 0 }.write(&mut buf);
        // big-endian, 첫 두 바이트는 고정.
        assert_eq!(
            buf,
            [0x90, 0x00, 0x12, 0x34, 0xDE, 0xAD, 0xBE, 0xEF, 0, 0, 0, 0]
        );
        assert_eq!(RtpHeader::read(&buf).unwrap().sequence_number, 0x1234);
        assert_eq!(RtpHeader::read(&buf).unwrap().timestamp, 0xDEADBEEF);
    }

    #[test]
    fn rtp_header_rejects_short_buffer() {
        assert!(RtpHeader::read(&[0u8; RTP_HEADER_SIZE - 1]).is_none());
    }

    #[test]
    fn nv_packet_golden_bytes() {
        let pkt = NvVideoPacket {
            stream_packet_index: 0x00_01_02_03,
            frame_index: 7,
            flags: rtp_flag::CONTAINS_PIC_DATA | rtp_flag::START_OF_FRAME,
            multi_fec_blocks: 0,
            fec_info: 0,
        };
        let mut buf = [0u8; NV_VIDEO_PACKET_SIZE];
        pkt.write(&mut buf);
        // spi 는 상위 24비트에 실린다: 0x00010203 << 8 = 0x01020300 (LE).
        assert_eq!(buf[0..4], [0x00, 0x03, 0x02, 0x01]);
        assert_eq!(buf[4..8], [7, 0, 0, 0]);
        assert_eq!(buf[9], 0);
        assert_eq!(buf[10], MULTI_FEC_FLAGS);
        // 24비트로 잘린 뒤 되읽힌다.
        assert_eq!(NvVideoPacket::read(&buf).unwrap().stream_packet_index, 0x00_01_02_03);
    }

    #[test]
    fn nv_packet_spi_truncates_to_24_bits() {
        let mut buf = [0u8; NV_VIDEO_PACKET_SIZE];
        NvVideoPacket { stream_packet_index: 0xAB_CD_EF_12, ..Default::default() }.write(&mut buf);
        // 상위 8비트(0xAB)는 shift-out 되어 사라진다.
        assert_eq!(NvVideoPacket::read(&buf).unwrap().stream_packet_index, 0x00_CD_EF_12);
    }

    #[test]
    fn fec_info_roundtrip() {
        for &(idx, data, pct) in &[(0usize, 1usize, 10usize), (37, 200, 60), (1023, 1023, 255)] {
            let mut buf = [0u8; NV_VIDEO_PACKET_SIZE];
            NvVideoPacket {
                fec_info: NvVideoPacket::pack_fec_info(idx, data, pct),
                ..Default::default()
            }
            .write(&mut buf);
            let got = NvVideoPacket::read(&buf).unwrap();
            assert_eq!((got.shard_index(), got.data_shards(), got.fec_percentage()), (idx, data, pct));
        }
    }

    #[test]
    fn multi_fec_blocks_roundtrip() {
        for block_count in 1..=MAX_FEC_BLOCKS {
            for block_index in 0..block_count {
                let mut buf = [0u8; NV_VIDEO_PACKET_SIZE];
                NvVideoPacket {
                    multi_fec_blocks: NvVideoPacket::pack_multi_fec_blocks(block_index, block_count),
                    ..Default::default()
                }
                .write(&mut buf);
                let got = NvVideoPacket::read(&buf).unwrap();
                assert_eq!((got.block_index(), got.block_count()), (block_index, block_count));
            }
        }
    }

    #[test]
    fn parity_shards_matches_host_formula() {
        let mk = |data: usize, pct: usize| NvVideoPacket {
            fec_info: NvVideoPacket::pack_fec_info(0, data, pct),
            ..Default::default()
        };
        assert_eq!(mk(100, 20).parity_shards(), 20);
        assert_eq!(mk(3, 10).parity_shards(), 1); // 0 으로 내려가지 않는다.
        // 나누어떨어지지 않으면 **올림** — moonlight-common-c `(data*pct+99)/100` 과 같다.
        assert_eq!(mk(7, 15).parity_shards(), 2);
        assert_eq!(mk(13, 25).parity_shards(), 4);
        // data + parity 는 MAX_SHARDS 를 넘지 않는다.
        assert_eq!(mk(213, 20).parity_shards(), 42);
        assert_eq!(mk(250, 60).parity_shards(), 5);
        // 큰 블록: 예전 floor 식이면 16, 올림이면 17 — 송/수신이 반드시 같은 쪽이어야 한다.
        assert_eq!(parity_shard_count(107, 15), 17);
        assert_eq!(mk(107, 15).parity_shards(), 17);
        // parity 없음 모드는 0 을 유지한다(1 로 올리면 클라이언트가 유령 shard 를 기다린다).
        assert_eq!(parity_shard_count(107, 0), 0);
        assert_eq!(mk(107, 0).parity_shards(), 0);
        assert_eq!(parity_shard_count(0, 20), 0);
    }

    /// 수신 측이 송신 측 개수를 **정확히** 재현하지 못하면 FEC 가 조용히 프레임을 망가뜨린다.
    /// 그래서 두 경로가 같은 함수를 쓰는지 전 조합으로 못박는다.
    #[test]
    fn parity_shard_count_round_trips_through_fec_info() {
        for data in 1..=MAX_SHARDS {
            for pct in 0..=60 {
                let want = parity_shard_count(data, pct);
                let nv = NvVideoPacket {
                    fec_info: NvVideoPacket::pack_fec_info(0, data, pct),
                    ..Default::default()
                };
                assert_eq!(nv.data_shards(), data);
                assert_eq!(nv.fec_percentage(), pct);
                assert_eq!(nv.parity_shards(), want, "data={data} pct={pct}");
                assert!(data + want <= MAX_SHARDS, "data={data} pct={pct} 가 상한을 넘었다");
            }
        }
    }

    #[test]
    fn frame_header_golden_bytes() {
        let mut buf = [0u8; VIDEO_FRAME_HEADER_SIZE];
        VideoFrameHeader { is_key_frame: true, frame_processing_latency: 0, last_payload_len: 512 }
            .write(&mut buf);
        assert_eq!(buf, [0x01, 0, 0, FRAME_TYPE_KEY, 0x00, 0x02, 0, 0]);

        VideoFrameHeader { is_key_frame: false, frame_processing_latency: 0, last_payload_len: 1 }
            .write(&mut buf);
        assert_eq!(buf[3], FRAME_TYPE_DELTA);

        let got = VideoFrameHeader::read(&buf).unwrap();
        assert!(!got.is_key_frame);
        assert_eq!(got.last_payload_len, 1);
    }

    #[test]
    fn shard_payload_len_derivation() {
        // packet_size 1392 → shard payload 1376 → 데이터그램 1408.
        assert_eq!(shard_payload_len(PAYLOAD_OFFSET + 1376), Some(1376));
        assert_eq!(shard_payload_len(PAYLOAD_OFFSET), None);
        assert_eq!(shard_payload_len(4), None);
    }

    fn fec_status(total_d: u16, total_p: u16, recv_d: u16, recv_p: u16) -> FrameFecStatus {
        FrameFecStatus {
            frame_index: 7,
            highest_received_seq: 41,
            next_contiguous_seq: 12,
            missing_before_highest: 3,
            total_data_shards: total_d,
            total_parity_shards: total_p,
            received_data_shards: recv_d,
            received_parity_shards: recv_p,
            fec_percentage: 20,
            block_index: 1,
            block_count: 2,
        }
    }

    #[test]
    fn fec_status_roundtrip() {
        let s = fec_status(100, 20, 90, 18);
        assert_eq!(FrameFecStatus::parse(&s.to_bytes()), Some(s));
    }

    #[test]
    fn fec_status_is_big_endian() {
        let b = fec_status(0x0102, 0x0304, 0x0506, 0x0708).to_bytes();
        assert_eq!(&b[0..4], &7u32.to_be_bytes());
        assert_eq!(&b[10..12], &[0x01, 0x02]);
        assert_eq!(&b[12..14], &[0x03, 0x04]);
        assert_eq!(b[18..21], [20, 1, 2]);
    }

    #[test]
    fn fec_status_rejects_short() {
        assert_eq!(FrameFecStatus::parse(&[0u8; FrameFecStatus::SIZE - 1]), None);
    }

    #[test]
    fn loss_fraction_matches_host_semantics() {
        assert_eq!(fec_status(100, 20, 100, 20).loss_fraction(), Some(0.0));
        assert_eq!(fec_status(100, 20, 50, 10).loss_fraction(), Some(0.5));
        assert_eq!(fec_status(100, 0, 90, 0).loss_fraction(), Some(0.1));
        assert_eq!(fec_status(0, 0, 0, 0).loss_fraction(), None);
        // 과다 보고여도 음수/overflow 없이 0.0.
        assert_eq!(fec_status(100, 20, 200, 50).loss_fraction(), Some(0.0));
    }

    #[test]
    fn rtt_probe_round_trips() {
        let probe = rtt_probe(1_234_567_890);
        assert_eq!(probe.len(), RTT_PROBE_SIZE);
        assert_eq!(&probe[..4], PONG_TAG);
        assert_eq!(parse_rtt_probe(&probe), Some(1_234_567_890));
    }

    #[test]
    fn rtt_probe_rejects_non_probes() {
        // PING(4B) 은 프로브가 아니다.
        assert_eq!(parse_rtt_probe(PING_PAYLOAD), None);
        // 태그가 달라도 거부.
        let mut wrong = rtt_probe(7);
        wrong[0] = b'X';
        assert_eq!(parse_rtt_probe(&wrong), None);
        // 실제 미디어 shard 는 헤더만으로도 프로브보다 크다 — 길이만으로 갈린다.
        assert!(PAYLOAD_OFFSET > RTT_PROBE_SIZE);
        assert_eq!(parse_rtt_probe(&[0u8; PAYLOAD_OFFSET]), None);
    }
}
