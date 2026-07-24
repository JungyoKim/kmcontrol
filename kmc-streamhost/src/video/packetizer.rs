//! GameStream 비디오 프레임 → 네트워크 shard 패킷화.
//!
//! Reed-Solomon FEC(기본 20% parity) + multi-FEC 블록으로 패킷 손실을 복구한다(무선/인터넷 경유 대비).
//! shard 레이아웃 (moonlight-common-c / Sunshine / moonshine 참조):
//!   [ RTP 헤더(12) | 패딩(4) | NvVideoPacket(16) | payload(≤ packet_size-16) ]
//! payload 스트림 = [ VideoFrameHeader(8) ++ encoded_NAL ] 을 shard 크기로 분할.
//!
//! 바이트 오더 주의: RTP 헤더는 big-endian, NvVideoPacket/VideoFrameHeader는 little-endian.

const NV_VIDEO_PACKET_SIZE: usize = 16;
const RTP_HEADER_SIZE: usize = 12;
const PADDING_SIZE: usize = 4;
const NV_PACKET_OFFSET: usize = RTP_HEADER_SIZE + PADDING_SIZE; // 16
const PAYLOAD_OFFSET: usize = NV_PACKET_OFFSET + NV_VIDEO_PACKET_SIZE; // 32
const VIDEO_FRAME_HEADER_SIZE: usize = 8;

#[repr(u8)]
enum RtpFlag {
    ContainsPicData = 0x1,
    EndOfFrame = 0x2,
    StartOfFrame = 0x4,
}

/// RTP 헤더를 shard 시작에 기록 (big-endian).
fn write_rtp_header(buf: &mut [u8], sequence_number: u16, timestamp: u32) {
    buf[0] = 0x90; // version/flags (Sunshine 고정값)
    buf[1] = 0; // packet_type
    buf[2..4].copy_from_slice(&sequence_number.to_be_bytes());
    buf[4..8].copy_from_slice(&timestamp.to_be_bytes());
    buf[8..12].copy_from_slice(&0u32.to_be_bytes()); // ssrc
}

/// NvVideoPacket 헤더 기록 (little-endian).
fn write_nv_video_packet(
    buf: &mut [u8],
    stream_packet_index: u32,
    frame_index: u32,
    flags: u8,
    multi_fec_blocks: u8,
    fec_info: u32,
) {
    buf[0..4].copy_from_slice(&stream_packet_index.to_le_bytes());
    buf[4..8].copy_from_slice(&frame_index.to_le_bytes());
    buf[8] = flags;
    buf[9] = 0; // reserved
    buf[10] = 0x10; // multi_fec_flags
    buf[11] = multi_fec_blocks;
    buf[12..16].copy_from_slice(&fec_info.to_le_bytes());
}

/// VideoFrameHeader (8바이트, little-endian) 직렬화.
fn write_video_frame_header(buf: &mut [u8; VIDEO_FRAME_HEADER_SIZE], is_key_frame: bool, last_payload_len: u32) {
    buf[0] = 0x01; // header_type
    buf[1..3].copy_from_slice(&0u16.to_le_bytes()); // frame_processing_latency
    buf[3] = if is_key_frame { 2 } else { 1 }; // frame_type
    buf[4..8].copy_from_slice(&last_payload_len.to_le_bytes());
}

/// [VideoFrameHeader ++ encoded_data] 논리 스트림에서 [offset, offset+len) 구간을 dst에 복사.
fn copy_header_and_data(
    dst: &mut [u8],
    header: &[u8; VIDEO_FRAME_HEADER_SIZE],
    encoded_data: &[u8],
    offset: usize,
    len: usize,
) {
    let total = VIDEO_FRAME_HEADER_SIZE + encoded_data.len();
    let end = (offset + len).min(total);
    let mut written = 0;
    if offset < VIDEO_FRAME_HEADER_SIZE {
        let header_end = VIDEO_FRAME_HEADER_SIZE.min(end);
        let n = header_end - offset;
        dst[written..written + n].copy_from_slice(&header[offset..header_end]);
        written += n;
        if end > VIDEO_FRAME_HEADER_SIZE {
            let n = end - VIDEO_FRAME_HEADER_SIZE;
            dst[written..written + n].copy_from_slice(&encoded_data[..n]);
        }
    } else {
        let data_start = offset - VIDEO_FRAME_HEADER_SIZE;
        let data_end = end - VIDEO_FRAME_HEADER_SIZE;
        dst[written..written + (data_end - data_start)]
            .copy_from_slice(&encoded_data[data_start..data_end]);
    }
}

/// 하나의 인코딩된 프레임을 shard(데이터+FEC parity) 벡터로 패킷화.
/// Reed-Solomon FEC 적용 (fec_percentage% parity). `sequence_number`는 RTP seq(데이터+parity 모두 증가),
/// `stream_packet_index`는 **데이터 패킷만** 증가하는 전역 카운터 (Moonlight 디패킷타이저 연속성 요구).
pub fn packetize_frame(
    encoded_data: &[u8],
    is_key_frame: bool,
    requested_packet_size: usize,
    frame_number: u32,
    sequence_number: &mut u32,
    stream_packet_index: &mut u32,
    rtp_timestamp: u32,
) -> Vec<Vec<u8>> {
    const FEC_PERCENTAGE: usize = 20;
    const MAX_SHARDS: usize = 255;

    let shard_payload_size = requested_packet_size - NV_VIDEO_PACKET_SIZE;
    let packet_data_len = VIDEO_FRAME_HEADER_SIZE + encoded_data.len();

    let last_shard_size = match packet_data_len % shard_payload_size {
        0 => shard_payload_size,
        n => n,
    };

    let mut header_bytes = [0u8; VIDEO_FRAME_HEADER_SIZE];
    write_video_frame_header(&mut header_bytes, is_key_frame, last_shard_size as u32);

    let total_data_shards = packet_data_len.div_ceil(shard_payload_size).max(1);
    let shard_size = PAYLOAD_OFFSET + shard_payload_size;

    // FEC 블록 분할 (moonshine/Sunshine): 블록당 데이터 shard 상한으로 4K 등 큰 프레임 대응.
    let nr_parity_per_block = MAX_SHARDS * FEC_PERCENTAGE / (100 + FEC_PERCENTAGE);
    let nr_data_per_block = MAX_SHARDS - nr_parity_per_block;
    // 최대 4블록 (multi_fec_blocks 2비트). 초과분은 마지막 블록에 몰아 FEC 없이.
    let nr_blocks = ((total_data_shards - 1) / nr_data_per_block + 1).min(4);
    let last_block_index = (nr_blocks as u8 - 1) << 6;

    let mut shards: Vec<Vec<u8>> = Vec::new();
    let mut seq = *sequence_number;

    for block_index in 0..nr_blocks {
        let start = block_index * nr_data_per_block;
        let end = if block_index == nr_blocks - 1 {
            total_data_shards // 마지막 블록은 남은 전부 (4블록 상한 시 초과분 포함).
        } else {
            ((block_index + 1) * nr_data_per_block).min(total_data_shards)
        };
        let block_data_shards = end - start;
        if block_data_shards == 0 {
            break;
        }

        let nr_parity = ((block_data_shards * FEC_PERCENTAGE / 100).max(1))
            .min(MAX_SHARDS.saturating_sub(block_data_shards));
        let fec_percentage = nr_parity * 100 / block_data_shards;
        let block_total = block_data_shards + nr_parity;
        let multi_fec_blocks: u8 = ((block_index as u8) << 4) | last_block_index;

        // 이 블록의 모든 shard를 0-초기화.
        let block_base = shards.len();
        for _ in 0..block_total {
            shards.push(vec![0u8; shard_size]);
        }

        // 데이터 shard 작성 (RTP+NvVideoPacket 헤더를 FEC 전에 완성).
        for i in 0..block_data_shards {
            let global_data_index = start + i;
            let payload_start = global_data_index * shard_payload_size;
            let payload_len = shard_payload_size.min(packet_data_len - payload_start);
            let cur_seq = seq + i as u32;
            let shard = &mut shards[block_base + i];

            write_rtp_header(shard, cur_seq as u16, rtp_timestamp);

            let mut flags = RtpFlag::ContainsPicData as u8;
            if global_data_index == 0 {
                flags |= RtpFlag::StartOfFrame as u8;
            }
            if global_data_index == total_data_shards - 1 {
                flags |= RtpFlag::EndOfFrame as u8;
            }
            let fec_info = (i << 12 | block_data_shards << 22 | fec_percentage << 4) as u32;
            // streamPacketIndex는 데이터 패킷만 세는 전역 카운터 (parity 제외 후 연속성 요구).
            let spi = *stream_packet_index;
            *stream_packet_index = stream_packet_index.wrapping_add(1);
            write_nv_video_packet(
                &mut shard[NV_PACKET_OFFSET..NV_PACKET_OFFSET + NV_VIDEO_PACKET_SIZE],
                spi << 8,
                frame_number,
                flags,
                multi_fec_blocks,
                fec_info,
            );
            copy_header_and_data(
                &mut shard[PAYLOAD_OFFSET..],
                &header_bytes,
                encoded_data,
                payload_start,
                payload_len,
            );
        }

        // Reed-Solomon parity (이 블록의 데이터 shard → parity).
        if nr_parity > 0 {
            let block_slice = &mut shards[block_base..block_base + block_total];
            match fec_rs::ReedSolomon::new(block_data_shards, nr_parity) {
                Ok(rs) => {
                    if rs.encode(block_slice).is_err() {
                        tracing::warn!("FEC encode failed; sending data shards only");
                    }
                }
                Err(e) => tracing::warn!("FEC encoder create failed: {e}"),
            }
        }

        // parity shard 헤더 패치.
        for i in 0..nr_parity {
            let block_shard_index = block_data_shards + i;
            let cur_seq = seq + block_shard_index as u32;
            let shard = &mut shards[block_base + block_shard_index];
            shard[0] = 0x90;
            shard[1] = 0;
            shard[2..4].copy_from_slice(&(cur_seq as u16).to_be_bytes());
            let nv = &mut shard[NV_PACKET_OFFSET..NV_PACKET_OFFSET + NV_VIDEO_PACKET_SIZE];
            nv[4..8].copy_from_slice(&frame_number.to_le_bytes());
            nv[11] = multi_fec_blocks;
            let fec_info = (block_shard_index << 12 | block_data_shards << 22 | fec_percentage << 4) as u32;
            nv[12..16].copy_from_slice(&fec_info.to_le_bytes());
        }

        seq += block_total as u32;
    }

    *sequence_number = seq;
    shards
}

#[cfg(test)]
mod tests {
    use super::*;

    // 테스트 헬퍼: SPI 카운터를 함께 전달.
    fn pf(data: &[u8], key: bool, psize: usize, fnum: u32, seq: &mut u32, ts: u32) -> Vec<Vec<u8>> {
        let mut spi = 0u32;
        packetize_frame(data, key, psize, fnum, seq, &mut spi, ts)
    }

    #[test]
    fn single_shard_small_frame() {
        let data = vec![0xAAu8; 100];
        let mut seq = 0u32;
        let shards = pf(&data, true, 1024, 1, &mut seq, 12345);
        // 100 + 8(header) = 108 < payload_size(1024-16=1008) → 1 데이터 shard + 1 parity(20%,min1).
        assert_eq!(shards.len(), 2);
        assert_eq!(seq, 2);
        let s = &shards[0];
        // RTP byte 0 고정값.
        assert_eq!(s[0], 0x90);
        // NvVideoPacket flags: SOF|EOF|PicData = 0x4|0x2|0x1 = 0x7.
        assert_eq!(s[NV_PACKET_OFFSET + 8], 0x7);
        // frame_index (LE) == 1 (데이터·parity 모두).
        let fi = u32::from_le_bytes(s[NV_PACKET_OFFSET + 4..NV_PACKET_OFFSET + 8].try_into().unwrap());
        assert_eq!(fi, 1);
        let fi_p = u32::from_le_bytes(shards[1][NV_PACKET_OFFSET + 4..NV_PACKET_OFFSET + 8].try_into().unwrap());
        assert_eq!(fi_p, 1);
    }

    #[test]
    fn multi_shard_frame_splits() {
        // payload_size = 256-16 = 240. data 500 + 8 = 508 → ceil(508/240)=3 데이터 shard + 1 parity.
        let data = vec![0x11u8; 500];
        let mut seq = 10u32;
        let shards = pf(&data, false, 256, 7, &mut seq, 999);
        assert_eq!(shards.len(), 4); // 3 data + 1 parity
        assert_eq!(seq, 14);
        // 첫 shard: SOF set, EOF unset.
        assert_eq!(shards[0][NV_PACKET_OFFSET + 8] & 0x4, 0x4);
        assert_eq!(shards[0][NV_PACKET_OFFSET + 8] & 0x2, 0x0);
        // 마지막 데이터 shard(index 2): EOF set.
        assert_eq!(shards[2][NV_PACKET_OFFSET + 8] & 0x2, 0x2);
        // 시퀀스 번호가 big-endian으로 증가 (데이터 shard 0 vs 2).
        let seq0 = u16::from_be_bytes(shards[0][2..4].try_into().unwrap());
        let seq2 = u16::from_be_bytes(shards[2][2..4].try_into().unwrap());
        assert_eq!(seq2 - seq0, 2);
    }

    #[test]
    fn frame_header_reconstructs_at_payload_start() {
        let data = vec![0x55u8; 50];
        let mut seq = 0u32;
        let shards = pf(&data, true, 1024, 1, &mut seq, 0);
        let payload = &shards[0][PAYLOAD_OFFSET..];
        // VideoFrameHeader: header_type=1, frame_type=2(key).
        assert_eq!(payload[0], 0x01);
        assert_eq!(payload[3], 0x02);
        // 그 뒤 encoded_data 시작.
        assert_eq!(payload[VIDEO_FRAME_HEADER_SIZE], 0x55);
    }

    #[test]
    fn fec_parity_shard_headers() {
        // 3 데이터 shard + 1 parity. parity shard의 Moonlight 필드가 올바른지 확인.
        let data = vec![0x33u8; 500];
        let mut seq = 0u32;
        let shards = pf(&data, false, 256, 42, &mut seq, 999);
        assert_eq!(shards.len(), 4); // 3 data + 1 parity
        let parity = &shards[3];
        // RTP byte 0 고정값 (FEC 후 패치됨).
        assert_eq!(parity[0], 0x90);
        // frame_index (LE) == 42.
        let fi = u32::from_le_bytes(parity[NV_PACKET_OFFSET + 4..NV_PACKET_OFFSET + 8].try_into().unwrap());
        assert_eq!(fi, 42);
        // fec_info: shard_index(3) << 12 | data_shards(3) << 22 | pct << 4.
        let fec_info = u32::from_le_bytes(parity[NV_PACKET_OFFSET + 12..NV_PACKET_OFFSET + 16].try_into().unwrap());
        assert_eq!((fec_info >> 12) & 0x3FF, 3); // shard index
        assert_eq!((fec_info >> 22) & 0x3FF, 3); // data shard count
        // parity shard 시퀀스 = base(0) + 3.
        let pseq = u16::from_be_bytes(parity[2..4].try_into().unwrap());
        assert_eq!(pseq, 3);
    }

    #[test]
    fn stream_packet_index_contiguous_over_data_only() {
        // SPI(streamPacketIndex >> 8)는 데이터 패킷만 세는 전역 +1 카운터여야 한다 (parity 제외).
        // 두 프레임을 연속 패킷화해 데이터 SPI가 프레임 경계·블록 경계에서 끊기지 않음을 검증.
        let mut seq = 0u32;
        let mut spi = 0u32;
        let read_spi = |shard: &[u8]| -> u32 {
            let raw = u32::from_le_bytes(shard[NV_PACKET_OFFSET..NV_PACKET_OFFSET + 4].try_into().unwrap());
            (raw >> 8) & 0xFFFFFF // Moonlight 디패킷타이저와 동일 마스킹
        };
        // 데이터 shard만 골라 SPI 수집 (parity는 flags에 PicData 없음 → EOF/SOF/0).
        let mut collected = Vec::new();
        for fnum in 1..=2u32 {
            let data = vec![0x22u8; 500]; // 256 packet_size → 3 데이터 + 1 parity
            let shards = packetize_frame(&data, fnum == 1, 256, fnum, &mut seq, &mut spi, fnum);
            let nr_data = 3;
            for (i, s) in shards.iter().enumerate() {
                if i < nr_data {
                    collected.push(read_spi(s));
                }
            }
        }
        // 6개 데이터 패킷: SPI = 0,1,2,3,4,5 연속 (parity가 소비하지 않음).
        assert_eq!(collected, vec![0, 1, 2, 3, 4, 5]);
    }
}
