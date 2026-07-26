//! 비디오 RTP 수신 → Reed-Solomon FEC 복구 → 액세스 유닛(AU) 재조립.
//!
//! 호스트 `kmc-streamhost/src/video/packetizer.rs` 의 **정확한 역함수**다. 와이어 레이아웃은
//! [`kmc_gsproto`] 가 단일 소스로 정의하고, 이 모듈은 그 위에서 "분할 정책의 역"만 수행한다:
//! shard 슬롯 배치 → 블록별 RS 복구 → 블록 순서대로 payload 연결 → 마지막 shard 패딩 트림.
//!
//! # 호스트와의 계약 (소스를 읽어서 확인한 것들)
//! - **PING**: 호스트는 비디오 UDP 소켓에서 정확히 `b"PING"` 4바이트만 클라이언트 등록 신호로
//!   받아들이고(`video/mod.rs`), 3초간 PING 이 없으면 인코딩을 멈춘다. 그래서 수신 루프가
//!   [`PING_INTERVAL`] 마다 PING 을 계속 쏴야 한다 (moonlight-common-c `udpPingThread` 도 500ms).
//! - **parity shard 식별**: 호스트는 RS 계산 *후* parity shard 의 헤더 **일부만** 덮어쓴다
//!   (`flags`/`stream_packet_index`/`timestamp`/`ssrc` 는 RS 가 계산한 값 그대로 남는다).
//!   따라서 parity 판별은 `flags` 가 아니라 `fec_info` 의 shard index 로 한다.
//! - **RS 를 payload 구간에만 적용**: 호스트는 shard **전체**(헤더 포함) 위에서 RS 를 계산하지만,
//!   RS 소거 부호는 바이트 열(column)마다 독립인 선형 연산이다 — 열 j 의 parity 는 열 j 의 데이터
//!   에만 의존한다. 그래서 모든 shard 에서 같은 구간([`PAYLOAD_OFFSET`] 이후)만 잘라 RS 를 돌려도
//!   결과가 동일하다. 덤으로 위에서 말한 "덮어써서 RS 값이 아니게 된 parity 헤더 바이트"(전부
//!   오프셋 32 미만)를 아예 건드리지 않게 되어 복구가 정확해진다.
//! - **parity 개수는 추정하지 않는다**: RS 생성행렬의 parity 행은 *총 parity 개수*에 의존한다
//!   (fec-rs `build_matrix(data+parity, data)` 의 부분행렬). `RS(13,3)` 이 만든 parity 를
//!   `RS(13,2)` 로 복구하면 **에러 없이 쓰레기**가 나온다 — 실측으로 확인했다. 그래서 개수는
//!   와이어의 `fec_percentage` 하나에서 [`kmc_gsproto::parity_shard_count`] 로 뽑고, 호스트도
//!   **같은 함수**를 쓴다. 관측한 인덱스에 맞춰 슬롯을 늘리는 식의 추정은 금지다.
//! - **`VideoFrameHeader`**: 논리 payload 스트림 맨 앞 8바이트에 **한 번만** 실린다(= 블록0 shard0).
//!   `last_payload_len` 은 **프레임 전체의 마지막 데이터 shard** 유효 길이이고 그 뒤는 0 패딩이다.
//!
//! # 출력 계약
//! [`AuFrame::data`] 는 self-framed 버퍼다: `data[0]` 이 타입 바이트(1=키프레임/0=델타)이고
//! 그 뒤가 시작코드 포함 Annex-B AU. 구 FFI 경로(`conn.rs`)가 만들던 것과 **동일한 형태**라
//! kmc-admin 팬아웃과 프론트 WebCodecs 는 바뀔 게 없다. 디코드는 여기서 하지 않는다.

use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use kmc_gsproto::{
    shard_payload_len, FrameFecStatus, NvVideoPacket, RtpHeader, VideoFrameHeader, MAX_FEC_BLOCKS,
    MAX_SHARDS, PAYLOAD_OFFSET, RTP_VERSION_FLAGS, VIDEO_FRAME_HEADER_SIZE,
};

/// 호스트가 클라이언트 주소 등록 신호로 받아들이는 유일한 payload
/// (`kmc-streamhost/src/video/mod.rs`: `if &recv_buf[..len] == b"PING"`).
pub const PING_PAYLOAD: &[u8] = b"PING";

/// PING 송신 주기. 호스트는 3초 무소식이면 인코딩을 멈추므로 6배 여유를 둔다
/// (moonlight-common-c `VideoStream.c` 의 ping 스레드와 같은 500ms).
pub const PING_INTERVAL: Duration = Duration::from_millis(500);

/// 비디오 UDP 기본 포트. 실제 포트는 RTSP 협상 결과를 쓴다 — 이 값은 문서용 기본값일 뿐이다.
pub const DEFAULT_VIDEO_PORT: u16 = 47998;

/// UDP 수신 버퍼. IDR 한 장이 수백 shard 버스트로 오므로 크게 잡는다. (송신 쪽은 반대로 작게 잡아야
/// bufferbloat 가 없지만, **수신**은 커널이 버스트를 흘리지 않도록 크게 잡는 것이 맞다.)
const RECV_BUFFER_BYTES: usize = 2 * 1024 * 1024;

/// 데이터그램 수신 버퍼 크기. 호스트 packet_size 는 MTU 이하(보통 1024~1392)다.
const MAX_DATAGRAM: usize = 2048;

/// 동시에 붙들 수 있는 미완성 프레임 수.
///
/// 호스트는 한 소켓에서 프레임을 **직렬로** 송출하므로(`for shard in shards { send_to().await }`)
/// 프레임 간 뒤섞임은 네트워크 재정렬(수 ms) 뿐이다. 60fps 기준 4프레임 ≒ 67ms — 그보다 오래
/// 붙들어봐야 지연만 쌓이고, 어차피 IDR 재요청이 더 빠른 복구 경로다.
pub const MAX_INFLIGHT_FRAMES: usize = 4;

/// 첫 shard 도착 후 이 시간이 지나도 못 채운 프레임은 포기한다. 한 프레임의 shard 는 같은 버스트로
/// 오므로 100ms 뒤에 남은 조각이 도착할 가능성은 사실상 없다(저프레임레이트에서도 안전한 상한).
pub const ASSEMBLY_BUDGET: Duration = Duration::from_millis(100);

/// 재조립 완료된 액세스 유닛.
///
/// `data` 는 self-framed: `data[0]` = 1(키프레임)/0(델타), 그 뒤는 시작코드 포함 Annex-B AU.
/// `keyframe` 은 편의용 사본(= `data[0] == 1`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuFrame {
    pub keyframe: bool,
    pub data: Vec<u8>,
}

/// 디패킷타이저가 뱉는 이벤트. 소켓 루프가 각 채널로 흘려보낸다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VideoEvent {
    /// 재조립 완료된 AU.
    Frame(AuFrame),
    /// FEC 로도 못 살려 버린 프레임. **호출자는 이 신호를 IDR 재요청으로 바꿔야 한다** —
    /// 참조 프레임이 끊겼으므로 다음 키프레임까지 화면이 깨진다.
    Lost { frame_index: u32 },
    /// 데이터 shard 손실이 있었던 FEC 블록의 상태. control 채널이 `SS_FRAME_FEC_STATUS`(0x5502)로
    /// 보내고 호스트 비트레이트 컨트롤러가 [`FrameFecStatus::loss_fraction`] 으로 소비한다.
    Fec(FrameFecStatus),
}

/// `x` 가 `y` 보다 앞선 시퀀스인가 (16비트 랩어라운드 고려). moonlight-common-c `isBefore16`.
#[inline]
fn is_before16(x: u16, y: u16) -> bool {
    (x.wrapping_sub(y) as i16) < 0
}

/// 한 FEC 블록의 조립 상태.
struct BlockAsm {
    data_shards: usize,
    /// 이 블록에 실제 적용된 parity 비율 %(`fec_info` 에 실려 온 값).
    fec_percentage: usize,
    /// `data_shards + parity_slots` 길이. 각 원소는 **payload 구간만** 담는다(모듈 주석 참조).
    slots: Vec<Option<Vec<u8>>>,
    /// 데이터그램으로 실제 받은 슬롯 표시 — RS 가 채운 슬롯과 구분해 중복/통계를 정확히 센다.
    received: Vec<bool>,
    received_data: u16,
    received_parity: u16,
    // --- 손실 통계 (moonlight-common-c `RtpVideoQueue` 와 동일 규칙) ---
    seen_first: bool,
    lowest_seq: u16,
    highest_seq: u16,
    next_contiguous_seq: u16,
    missing_before_highest: u16,
    fast_path: bool,
}

impl BlockAsm {
    fn new(data_shards: usize, parity_slots: usize, fec_percentage: usize) -> Self {
        let total = data_shards + parity_slots;
        Self {
            data_shards,
            fec_percentage,
            slots: vec![None; total],
            received: vec![false; total],
            received_data: 0,
            received_parity: 0,
            seen_first: false,
            lowest_seq: 0,
            highest_seq: 0,
            next_contiguous_seq: 0,
            missing_before_highest: 0,
            fast_path: true,
        }
    }

    fn parity_slots(&self) -> usize {
        self.slots.len() - self.data_shards
    }

    /// 시퀀스 기반 손실 통계 갱신 (새로 받은(중복 아닌) 패킷에 대해서만 호출).
    fn note_seq(&mut self, seq: u16, shard_index: usize) {
        if !self.seen_first {
            self.seen_first = true;
            self.lowest_seq = seq.wrapping_sub(shard_index as u16);
            self.next_contiguous_seq = self.lowest_seq;
            self.highest_seq = seq;
            self.missing_before_highest = seq.wrapping_sub(self.lowest_seq);
        } else if is_before16(self.highest_seq, seq) {
            self.missing_before_highest = self
                .missing_before_highest
                .wrapping_add(seq.wrapping_sub(self.highest_seq).wrapping_sub(1));
            self.highest_seq = seq;
        } else {
            // 최고 시퀀스보다 뒤 = 앞서 비어 있던 자리를 메운 것.
            self.missing_before_highest = self.missing_before_highest.saturating_sub(1);
        }

        if self.fast_path && seq == self.next_contiguous_seq {
            self.next_contiguous_seq = self.next_contiguous_seq.wrapping_add(1);
        } else if seq != self.next_contiguous_seq {
            // 순서가 깨진 순간부터 연속 카운터는 멈춘다 (C 구현의 useFastQueuePath 와 동일).
            self.fast_path = false;
        }
    }

    fn data_complete(&self) -> bool {
        self.slots[..self.data_shards].iter().all(Option::is_some)
    }

    /// 데이터 shard 가 모자라면 RS 로 복구한다. 데이터가 다 채워졌으면 `true`.
    fn try_recover(&mut self) -> bool {
        if self.data_complete() {
            return true;
        }
        let parity = self.parity_slots();
        if parity == 0 {
            return false;
        }
        if self.slots.iter().filter(|s| s.is_some()).count() < self.data_shards {
            return false; // 아직 복구 임계에 못 미침 — 더 기다린다.
        }
        let rs = match fec_rs::ReedSolomon::new(self.data_shards, parity) {
            Ok(rs) => rs,
            Err(e) => {
                tracing::warn!(data = self.data_shards, parity, error = %e, "FEC decoder create failed");
                return false;
            }
        };
        match rs.reconstruct_data(&mut self.slots) {
            Ok(()) => self.data_complete(),
            Err(e) => {
                tracing::debug!(data = self.data_shards, parity, error = %e, "FEC reconstruct failed");
                false
            }
        }
    }

    fn fec_status(
        &self,
        frame_index: u32,
        block_index: usize,
        block_count: usize,
    ) -> FrameFecStatus {
        FrameFecStatus {
            frame_index,
            highest_received_seq: self.highest_seq,
            next_contiguous_seq: self.next_contiguous_seq,
            missing_before_highest: self.missing_before_highest,
            total_data_shards: self.data_shards as u16,
            total_parity_shards: self.parity_slots() as u16,
            received_data_shards: self.received_data,
            received_parity_shards: self.received_parity,
            fec_percentage: self.fec_percentage.min(u8::MAX as usize) as u8,
            block_index: block_index as u8,
            block_count: block_count as u8,
        }
    }
}

/// 한 프레임(= 여러 FEC 블록)의 조립 상태.
struct FrameAsm {
    frame_index: u32,
    first_seen: Instant,
    block_count: usize,
    /// `multi_fec_blocks` 의 블록 인덱스가 2비트라 최대 [`MAX_FEC_BLOCKS`] 개.
    blocks: [Option<BlockAsm>; MAX_FEC_BLOCKS],
    /// 이 프레임 모든 shard 의 payload 길이(호스트가 프레임 내내 고정으로 쓴다).
    shard_payload_len: usize,
    /// AU(또는 Lost)를 이미 소비자에게 내보냈다. 마감 전까지 지각 shard 는 통계로만 흡수한다.
    delivered: bool,
}

impl FrameAsm {
    fn new(frame_index: u32, block_count: usize, shard_payload_len: usize, now: Instant) -> Self {
        Self {
            frame_index,
            first_seen: now,
            block_count,
            blocks: [const { None }; MAX_FEC_BLOCKS],
            shard_payload_len,
            delivered: false,
        }
    }

    fn complete(&self) -> bool {
        (0..self.block_count).all(|b| self.blocks[b].as_ref().is_some_and(BlockAsm::data_complete))
    }

    /// 블록들을 논리 순서(블록 → 블록 내 데이터 shard)로 이어 붙이고 패딩을 잘라 AU 를 만든다.
    fn assemble(&self) -> Option<AuFrame> {
        let plen = self.shard_payload_len;
        if plen < VIDEO_FRAME_HEADER_SIZE {
            return None; // 프레임 헤더도 못 담는 shard 크기 — 우리 호스트가 낼 수 없는 값.
        }
        let mut total_data = 0usize;
        for b in 0..self.block_count {
            total_data += self.blocks[b].as_ref()?.data_shards;
        }
        if total_data == 0 {
            return None;
        }

        let hdr = VideoFrameHeader::read(self.blocks[0].as_ref()?.slots[0].as_ref()?)?;
        let last = hdr.last_payload_len as usize;
        if last == 0 || last > plen {
            tracing::debug!(frame = self.frame_index, last, plen, "bogus last_payload_len");
            return None;
        }
        // 논리 스트림 길이 = [VideoFrameHeader(8) ++ AU]. 마지막 shard 뒤 0 패딩은 여기서 잘린다.
        let logical = (total_data - 1) * plen + last;
        if logical <= VIDEO_FRAME_HEADER_SIZE {
            return None; // AU 가 비었다 — 우리 인코더가 낼 수 없는 값.
        }

        // data[0] 은 타입 바이트 자리. admin 은 이 버퍼를 그대로 브라우저로 흘린다(재프레이밍 없음).
        let mut data = Vec::with_capacity(1 + logical - VIDEO_FRAME_HEADER_SIZE);
        data.push(u8::from(hdr.is_key_frame));
        let mut off = 0usize; // 논리 스트림 오프셋
        for b in 0..self.block_count {
            let blk = self.blocks[b].as_ref()?;
            for slot in &blk.slots[..blk.data_shards] {
                let p = slot.as_ref()?;
                let (start, end) = (off, off + p.len());
                off = end;
                // [8, logical) 과의 교집합만 복사 = 프레임 헤더 스킵 + 꼬리 패딩 트림.
                let lo = start.max(VIDEO_FRAME_HEADER_SIZE);
                let hi = end.min(logical);
                if lo < hi {
                    data.extend_from_slice(&p[lo - start..hi - start]);
                }
            }
        }
        Some(AuFrame { keyframe: hdr.is_key_frame, data })
    }
}

/// 상태 기계 본체 — 소켓과 무관하게 데이터그램만 먹고 이벤트를 뱉는다(그래서 단위 테스트가 쉽다).
///
/// 프레임은 **frame_index 오름차순으로만** 나간다. 한 프레임이 완성되면 그보다 오래된 미완성
/// 프레임은 그 자리에서 폐기하고([`VideoEvent::Lost`]), 마감 워터마크 아래의 지각 shard 는 버린다.
///
/// AU 전달과 프레임 **마감**은 시점이 다르다 — 전달은 즉시, 마감(= FEC 통계 보고)은 더 새로운
/// 프레임이 완성되거나 [`ASSEMBLY_BUDGET`] 이 지날 때. 이유는 [`Depacketizer::emit_completed`] 참조.
pub struct Depacketizer {
    frames: BTreeMap<u32, FrameAsm>,
    /// 이미 마감한 프레임 인덱스의 최대값. 이 아래 shard 는 지각으로 보고 버린다.
    last_retired: Option<u32>,
    events: VecDeque<VideoEvent>,
}

impl Default for Depacketizer {
    fn default() -> Self {
        Self::new()
    }
}

impl Depacketizer {
    pub fn new() -> Self {
        Self { frames: BTreeMap::new(), last_retired: None, events: VecDeque::new() }
    }

    /// 뱉을 이벤트를 하나 꺼낸다.
    pub fn pop_event(&mut self) -> Option<VideoEvent> {
        self.events.pop_front()
    }

    /// 데이터그램 하나를 먹는다. 형식이 안 맞으면 조용히 버린다(스트레이 트래픽 방어).
    pub fn push(&mut self, datagram: &[u8], now: Instant) {
        let Some(plen) = shard_payload_len(datagram.len()) else {
            return;
        };
        if datagram[0] != RTP_VERSION_FLAGS {
            return; // 우리 호스트는 데이터/parity 모두 첫 바이트를 0x90 으로 패치해서 보낸다.
        }
        let (Some(rtp), Some(nv)) =
            (RtpHeader::read(datagram), NvVideoPacket::read_from_shard(datagram))
        else {
            return;
        };

        let block_count = nv.block_count();
        let block_index = nv.block_index();
        let data_shards = nv.data_shards();
        let shard_index = nv.shard_index();
        if block_index >= block_count
            || data_shards == 0
            || data_shards > MAX_SHARDS
            || shard_index >= MAX_SHARDS
        {
            return;
        }

        let frame_index = nv.frame_index;
        if self.last_retired.is_some_and(|c| frame_index <= c) {
            return; // 이미 마감한 프레임의 지각 shard.
        }

        // 새 프레임이면 자리를 만든다(가득 찼으면 가장 오래된 것을 마감해 자리를 비운다).
        if !self.frames.contains_key(&frame_index) {
            if self.frames.len() >= MAX_INFLIGHT_FRAMES {
                let oldest = *self.frames.keys().next().expect("non-empty");
                if oldest > frame_index {
                    return; // 새로 온 게 오히려 더 옛날 — 무시.
                }
                let f = self.frames.remove(&oldest).expect("just looked up");
                self.retire(f);
            }
            self.frames.insert(frame_index, FrameAsm::new(frame_index, block_count, plen, now));
        }

        let frame = self.frames.get_mut(&frame_index).expect("inserted above");
        if frame.shard_payload_len != plen || frame.block_count != block_count {
            return; // 프레임 내 불일치 — RS 는 균일한 shard 길이를 요구한다.
        }
        let delivered = frame.delivered;

        // parity 개수는 호스트와 **같은 함수**(`gsproto::parity_shard_count`)로 뽑는다. RS 생성행렬의
        // parity 행은 총 parity 개수에 의존하므로, 하나라도 어긋나면 복구가 조용히 쓰레기를 낸다.
        let blk = frame.blocks[block_index].get_or_insert_with(|| {
            BlockAsm::new(data_shards, nv.parity_shards(), nv.fec_percentage())
        });
        if blk.data_shards != data_shards || shard_index >= blk.slots.len() {
            return; // 손상 or 우리가 계산한 블록 크기 밖 — 받으면 RS 입력이 깨진다.
        }
        if blk.received[shard_index] {
            return; // 중복.
        }

        blk.received[shard_index] = true;
        blk.note_seq(rtp.sequence_number, shard_index);
        if shard_index < data_shards {
            blk.received_data += 1;
        } else {
            blk.received_parity += 1;
        }
        blk.slots[shard_index] = Some(datagram[PAYLOAD_OFFSET..].to_vec());

        if delivered {
            return; // 이미 내보낸 프레임 — 지각 shard 는 손실 통계에만 반영한다.
        }
        if !blk.data_complete() {
            blk.try_recover();
        }
        if frame.complete() {
            self.emit_completed(frame_index);
        }
    }

    /// 시간 예산을 넘긴 프레임을 마감한다. 소켓 루프가 PING 틱마다 호출한다.
    /// 이미 AU 를 내보낸 프레임도 여기서 마감된다(지각 parity 를 다 세고 나서 통계를 낸다).
    pub fn expire(&mut self, now: Instant) {
        let stale: Vec<u32> = self
            .frames
            .iter()
            .filter(|(_, f)| now.saturating_duration_since(f.first_seen) > ASSEMBLY_BUDGET)
            .map(|(k, _)| *k)
            .collect();
        for k in stale {
            let f = self.frames.remove(&k).expect("just listed");
            self.retire(f);
        }
    }

    /// 완성된 프레임의 AU 를 **즉시** 내보낸다. 프레임 슬롯 자체는 남겨 두고, 그보다 오래된
    /// 프레임만 여기서 마감한다.
    ///
    /// 전달과 마감을 쪼개는 이유: 호스트는 parity 를 데이터 shard **뒤에** 보내므로, FEC 로 살린
    /// 프레임은 "복구에 필요한 최소 개수"가 도착한 순간 완성된다 — 남은 parity 는 아직 비행 중이다.
    /// 그 시점에 통계를 내면 곧 도착할 parity 까지 손실로 세어 호스트 비트레이트 컨트롤러
    /// (`bitrate.on_loss`)에 과장된 손실률을 먹인다. 그래서 화면 지연은 그대로 두고(AU 는 지금
    /// 나간다) 통계만 늦춘다.
    fn emit_completed(&mut self, frame_index: u32) {
        let older: Vec<u32> = self.frames.range(..frame_index).map(|(k, _)| *k).collect();
        for k in older {
            let f = self.frames.remove(&k).expect("just listed");
            self.retire(f);
        }
        let frame = self.frames.get_mut(&frame_index).expect("caller checked");
        frame.delivered = true;
        match frame.assemble() {
            Some(au) => self.events.push_back(VideoEvent::Frame(au)),
            None => {
                tracing::warn!(frame = frame_index, "complete frame failed to assemble");
                self.events.push_back(VideoEvent::Lost { frame_index });
            }
        }
    }

    /// 프레임을 최종 마감한다: 손실이 있었으면 FEC 상태를 보고하고, 아직 아무것도 못 내보낸
    /// 프레임이면 [`VideoEvent::Lost`] 를 띄운 뒤 마감 워터마크를 올린다.
    fn retire(&mut self, frame: FrameAsm) {
        let frame_index = frame.frame_index;
        let lost = !frame.delivered;
        if lost {
            tracing::debug!(frame = frame_index, "video frame unrecoverable; dropping");
        }
        self.report_fec(&frame, lost);
        if lost {
            self.events.push_back(VideoEvent::Lost { frame_index });
        }
        self.last_retired = Some(match self.last_retired {
            Some(c) if c > frame_index => c,
            _ => frame_index,
        });
    }

    /// 데이터 shard 손실이 있었던 블록만 보고한다.
    ///
    /// 무손실인데도 보고하면 안 된다: 호스트 비트레이트 컨트롤러(`bitrate.on_loss`)가 매 프레임
    /// 곱셈 감소를 먹는다. moonlight-common-c 도 "복구가 필요했을 때"와 "프레임을 버릴 때"만
    /// `reportFinalFrameFecStatus` 를 부른다.
    fn report_fec(&mut self, frame: &FrameAsm, lost: bool) {
        // 통째로 사라진 블록의 총량을 빌려올 형제 블록(호스트는 블록을 균등 분할한다).
        let template = (0..frame.block_count).find_map(|b| frame.blocks[b].as_ref());
        for b in 0..frame.block_count {
            match frame.blocks[b].as_ref() {
                Some(blk) => {
                    if (blk.received_data as usize) < blk.data_shards {
                        let st = blk.fec_status(frame.frame_index, b, frame.block_count);
                        self.events.push_back(VideoEvent::Fec(st));
                    }
                }
                None if lost => {
                    // 한 조각도 못 받은 블록 = 전손. 형제 블록의 총량으로 근사해 신호를 살린다.
                    if let Some(t) = template {
                        let mut st = t.fec_status(frame.frame_index, b, frame.block_count);
                        st.received_data_shards = 0;
                        st.received_parity_shards = 0;
                        st.highest_received_seq = 0;
                        st.next_contiguous_seq = 0;
                        st.missing_before_highest = st.total_data_shards + st.total_parity_shards;
                        self.events.push_back(VideoEvent::Fec(st));
                    }
                }
                None => {}
            }
        }
    }
}

/// 비디오 UDP 수신 루프.
///
/// `local_bind` 에 소켓을 열고 `host_video_addr`(RTSP 협상으로 받은 포트, 기본
/// [`DEFAULT_VIDEO_PORT`])로 [`PING_PAYLOAD`] 를 즉시 + [`PING_INTERVAL`] 주기로 보낸다.
/// 호스트는 이 PING 으로 클라이언트 주소를 등록하고, 3초 끊기면 인코딩을 멈춘다.
///
/// - `au_tx`: 재조립된 AU. admin 팬아웃이 그대로 소비한다(self-framed `data`).
/// - `fec_tx`: 손실이 있었던 블록의 `SS_FRAME_FEC_STATUS`. control 채널이 0x5502 로 전송한다.
/// - `lost_tx`: 복구 못 한 프레임 인덱스. 호출자는 이걸 IDR 재요청으로 바꾼다.
/// - `shutdown`: `true` 가 되면 종료한다. 패킷이 흐르는 동안은 즉시, 조용하면 최대
///   [`PING_INTERVAL`] 안에 관측된다.
///
/// `au_tx` 가 닫히면(수신자 소멸) 정상 종료한다.
pub async fn run(
    local_bind: SocketAddr,
    host_video_addr: SocketAddr,
    au_tx: std::sync::mpsc::Sender<AuFrame>,
    fec_tx: tokio::sync::mpsc::UnboundedSender<FrameFecStatus>,
    lost_tx: tokio::sync::mpsc::UnboundedSender<u32>,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    let socket = tokio::net::UdpSocket::bind(local_bind)
        .await
        .with_context(|| format!("bind video udp {local_bind}"))?;
    {
        let sref = socket2::SockRef::from(&socket);
        if let Err(e) = sref.set_recv_buffer_size(RECV_BUFFER_BYTES) {
            tracing::warn!(error = %e, "failed to set UDP recv buffer");
        }
    }
    socket
        .send_to(PING_PAYLOAD, host_video_addr)
        .await
        .context("send initial video PING")?;
    tracing::info!(%local_bind, %host_video_addr, "video receiver started (PING sent)");

    // 첫 틱은 방금 보낸 PING 이 대신하므로 한 주기 뒤부터.
    let mut ping =
        tokio::time::interval_at(tokio::time::Instant::now() + PING_INTERVAL, PING_INTERVAL);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut buf = vec![0u8; MAX_DATAGRAM];
    let mut dp = Depacketizer::new();

    while !shutdown.load(Ordering::Relaxed) {
        tokio::select! {
            r = socket.recv_from(&mut buf) => match r {
                Ok((len, from)) => {
                    if from.ip() != host_video_addr.ip() {
                        continue; // 호스트가 아닌 곳에서 온 패킷.
                    }
                    dp.push(&buf[..len], Instant::now());
                }
                Err(e) => {
                    // Windows WSAECONNRESET(10054): 앞서 보낸 PING 에 대한 ICMP port-unreachable
                    // 반송(호스트 소켓이 아직 안 떴을 때). 소켓 자체는 멀쩡하므로 계속 간다.
                    if e.raw_os_error() == Some(10054) {
                        continue;
                    }
                    return Err(e).context("video udp recv");
                }
            },
            _ = ping.tick() => {
                if let Err(e) = socket.send_to(PING_PAYLOAD, host_video_addr).await {
                    tracing::warn!(error = %e, "video PING send failed");
                }
                dp.expire(Instant::now());
            }
        }

        while let Some(ev) = dp.pop_event() {
            match ev {
                VideoEvent::Frame(au) => {
                    if au_tx.send(au).is_err() {
                        tracing::info!("AU sink closed; stopping video receiver");
                        return Ok(());
                    }
                }
                VideoEvent::Fec(st) => {
                    let _ = fec_tx.send(st);
                }
                VideoEvent::Lost { frame_index } => {
                    let _ = lost_tx.send(frame_index);
                }
            }
        }
    }
    tracing::info!("video receiver stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kmc_gsproto::{rtp_flag, NV_PACKET_OFFSET, NV_VIDEO_PACKET_SIZE};

    // ------------------------------------------------------------------
    // 호스트 패킷타이저의 **바이트 레이아웃 구성을 그대로 옮긴 것**
    // (kmc-streamhost/src/video/packetizer.rs, tracing 만 제거).
    // 라운드트립을 이 산출물 기준으로 검증하므로, 호스트 레이아웃이 바뀌면
    // 여기 복사본과 실제 호스트가 갈라지고 통합에서 즉시 드러난다.
    // ------------------------------------------------------------------

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

    fn host_packetize(
        encoded_data: &[u8],
        is_key_frame: bool,
        requested_packet_size: usize,
        frame_number: u32,
        first_seq: u32,
        fec_percentage: usize,
    ) -> Vec<Vec<u8>> {
        let fec_percentage = fec_percentage.clamp(10, 60);
        let shard_payload_size = requested_packet_size - NV_VIDEO_PACKET_SIZE;
        let packet_data_len = VIDEO_FRAME_HEADER_SIZE + encoded_data.len();
        let last_shard_size = match packet_data_len % shard_payload_size {
            0 => shard_payload_size,
            n => n,
        };
        let mut header_bytes = [0u8; VIDEO_FRAME_HEADER_SIZE];
        VideoFrameHeader {
            is_key_frame,
            frame_processing_latency: 0,
            last_payload_len: last_shard_size as u32,
        }
        .write(&mut header_bytes);

        let total_data_shards = packet_data_len.div_ceil(shard_payload_size).max(1);
        let shard_size = PAYLOAD_OFFSET + shard_payload_size;
        let nr_parity_per_block = MAX_SHARDS * fec_percentage / (100 + fec_percentage);
        let nr_data_per_block = MAX_SHARDS - nr_parity_per_block;
        let nr_blocks = ((total_data_shards - 1) / nr_data_per_block + 1).min(MAX_FEC_BLOCKS);

        let mut shards: Vec<Vec<u8>> = Vec::new();
        let mut seq = first_seq;
        let mut spi = 0u32;

        for block_index in 0..nr_blocks {
            let start = block_index * nr_data_per_block;
            let end = if block_index == nr_blocks - 1 {
                total_data_shards
            } else {
                ((block_index + 1) * nr_data_per_block).min(total_data_shards)
            };
            let block_data_shards = end - start;
            if block_data_shards == 0 {
                break;
            }
            // 호스트와 **동일**: 개수는 공유 함수로, 와이어에는 요청한 퍼센트를 그대로.
            let nr_parity = kmc_gsproto::parity_shard_count(block_data_shards, fec_percentage);
            let block_total = block_data_shards + nr_parity;
            let multi_fec_blocks = NvVideoPacket::pack_multi_fec_blocks(block_index, nr_blocks);

            let block_base = shards.len();
            for _ in 0..block_total {
                shards.push(vec![0u8; shard_size]);
            }

            for i in 0..block_data_shards {
                let global_data_index = start + i;
                let payload_start = global_data_index * shard_payload_size;
                let payload_len = shard_payload_size.min(packet_data_len - payload_start);
                let cur_seq = seq + i as u32;
                let shard = &mut shards[block_base + i];
                RtpHeader { sequence_number: cur_seq as u16, timestamp: 0, ssrc: 0 }.write(shard);
                let mut flags = rtp_flag::CONTAINS_PIC_DATA;
                if global_data_index == 0 {
                    flags |= rtp_flag::START_OF_FRAME;
                }
                if global_data_index == total_data_shards - 1 {
                    flags |= rtp_flag::END_OF_FRAME;
                }
                let cur_spi = spi;
                spi = spi.wrapping_add(1);
                NvVideoPacket {
                    stream_packet_index: cur_spi,
                    frame_index: frame_number,
                    flags,
                    multi_fec_blocks,
                    fec_info: NvVideoPacket::pack_fec_info(i, block_data_shards, fec_percentage),
                }
                .write(&mut shard[NV_PACKET_OFFSET..]);
                copy_header_and_data(
                    &mut shard[PAYLOAD_OFFSET..],
                    &header_bytes,
                    encoded_data,
                    payload_start,
                    payload_len,
                );
            }

            if nr_parity > 0 {
                let block_slice = &mut shards[block_base..block_base + block_total];
                let rs = fec_rs::ReedSolomon::new(block_data_shards, nr_parity).unwrap();
                rs.encode(block_slice).unwrap();
            }

            // parity 헤더 **부분** 패치 — 나머지 바이트는 RS 계산값 그대로 둔다(호스트와 동일).
            for i in 0..nr_parity {
                let block_shard_index = block_data_shards + i;
                let cur_seq = seq + block_shard_index as u32;
                let shard = &mut shards[block_base + block_shard_index];
                shard[0] = RTP_VERSION_FLAGS;
                shard[1] = 0;
                shard[2..4].copy_from_slice(&(cur_seq as u16).to_be_bytes());
                let nv = &mut shard[NV_PACKET_OFFSET..NV_PACKET_OFFSET + NV_VIDEO_PACKET_SIZE];
                nv[4..8].copy_from_slice(&frame_number.to_le_bytes());
                nv[11] = multi_fec_blocks;
                let fec_info = NvVideoPacket::pack_fec_info(
                    block_shard_index,
                    block_data_shards,
                    fec_percentage,
                );
                nv[12..16].copy_from_slice(&fec_info.to_le_bytes());
            }
            seq += block_total as u32;
        }
        shards
    }

    /// 결정적 의사난수 NAL (Annex-B 시작코드 + 페이로드).
    fn make_nal(len: usize, salt: u8) -> Vec<u8> {
        assert!(len >= 5);
        let mut v = Vec::with_capacity(len);
        v.extend_from_slice(&[0, 0, 0, 1, 0x65]);
        let mut x = salt as u32 | 1;
        while v.len() < len {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            // 0 은 피한다 — 패딩 트림 검증이 "마지막 바이트가 0인가"에 의미를 갖도록.
            v.push(((x >> 16) as u8) | 1);
        }
        v.truncate(len);
        v
    }

    fn packetize(nal: &[u8], key: bool, pkt: usize, frame: u32, pct: usize) -> Vec<Vec<u8>> {
        host_packetize(nal, key, pkt, frame, 0, pct)
    }

    /// `drop` 에 든 인덱스만 빼고 디패킷타이저에 먹인다.
    fn feed(dp: &mut Depacketizer, shards: &[Vec<u8>], drop: &[usize], now: Instant) {
        for (i, s) in shards.iter().enumerate() {
            if !drop.contains(&i) {
                dp.push(s, now);
            }
        }
    }

    fn events(dp: &mut Depacketizer) -> Vec<VideoEvent> {
        std::iter::from_fn(|| dp.pop_event()).collect()
    }

    fn only_frame(evs: &[VideoEvent]) -> &AuFrame {
        let mut it = evs.iter().filter_map(|e| match e {
            VideoEvent::Frame(f) => Some(f),
            _ => None,
        });
        let f = it.next().expect("expected exactly one Frame event");
        assert!(it.next().is_none(), "more than one Frame event");
        f
    }

    fn fec_statuses(evs: &[VideoEvent]) -> Vec<FrameFecStatus> {
        evs.iter()
            .filter_map(|e| match e {
                VideoEvent::Fec(s) => Some(*s),
                _ => None,
            })
            .collect()
    }

    // ------------------------------------------------------------------

    #[test]
    fn single_shard_keyframe_round_trip() {
        // packet_size 1024 → shard payload 1008. 200바이트 NAL 은 shard 하나에 다 들어간다.
        let nal = make_nal(200, 7);
        let shards = packetize(&nal, true, 1024, 1, 20);
        assert_eq!(shards.len(), 2, "1 data + 1 parity 여야 한다");

        let mut dp = Depacketizer::new();
        feed(&mut dp, &shards, &[], Instant::now());
        let evs = events(&mut dp);

        let au = only_frame(&evs);
        assert!(au.keyframe);
        assert_eq!(au.data[0], 1, "키프레임 타입 바이트");
        assert_eq!(&au.data[1..], &nal[..], "AU 바이트가 원본과 달라졌다");
        assert!(fec_statuses(&evs).is_empty(), "무손실인데 FEC 상태를 보고했다");
    }

    #[test]
    fn multi_shard_delta_frame_round_trip() {
        // 5000바이트 → shard payload 1008 기준 데이터 shard 5개.
        let nal = make_nal(5000, 3);
        let shards = packetize(&nal, false, 1024, 42, 20);
        assert!(shards.len() > 2, "여러 shard 로 쪼개져야 유효한 테스트");

        let mut dp = Depacketizer::new();
        feed(&mut dp, &shards, &[], Instant::now());
        let evs = events(&mut dp);

        let au = only_frame(&evs);
        assert!(!au.keyframe);
        assert_eq!(au.data[0], 0, "델타 프레임 타입 바이트");
        assert_eq!(&au.data[1..], &nal[..]);
    }

    #[test]
    fn padding_is_trimmed_exactly() {
        // packet_data_len = 8 + 1500 = 1508 = 1008 + 500 → 마지막 shard 는 500바이트만 유효.
        // 트림이 없으면 뒤에 508바이트 0 패딩이 붙는다.
        let nal = make_nal(1500, 11);
        let shards = packetize(&nal, false, 1024, 1, 20);
        let mut dp = Depacketizer::new();
        feed(&mut dp, &shards, &[], Instant::now());
        let evs = events(&mut dp);
        let au = only_frame(&evs);
        assert_eq!(au.data.len(), 1 + 1500, "패딩이 남았거나 과하게 잘렸다");
        assert_eq!(&au.data[1..], &nal[..]);
        assert_ne!(*au.data.last().unwrap(), 0, "트림 검증이 의미를 가지려면 끝이 0이 아니어야");
    }

    #[test]
    fn multi_fec_block_round_trip() {
        // packet_size 64 → shard payload 48. fec 10% 면 블록당 데이터 shard 상한 232.
        // 12000바이트 → 12008/48 = 251 shard → 2블록(232 + 19). 블록 연결 순서를 검증한다.
        let nal = make_nal(12_000, 5);
        let shards = packetize(&nal, true, 64, 9, 10);
        let probe = NvVideoPacket::read_from_shard(&shards[0]).unwrap();
        assert_eq!(probe.block_count(), 2, "멀티 FEC 블록이 나와야 유효한 테스트");

        let mut dp = Depacketizer::new();
        feed(&mut dp, &shards, &[], Instant::now());
        let evs = events(&mut dp);
        let au = only_frame(&evs);
        assert_eq!(au.data[0], 1);
        assert_eq!(&au.data[1..], &nal[..]);
    }

    #[test]
    fn fec_recovers_up_to_parity_data_shards() {
        // 6 데이터 shard, 50% → parity 3. 데이터 3장을 버려도 정확히 복구돼야 한다.
        let nal = make_nal(6 * 48 - 8, 13); // packet_size 64 기준 정확히 6 shard
        let shards = packetize(&nal, false, 64, 4, 50);
        assert_eq!(shards.len(), 9, "6 data + 3 parity");

        let mut dp = Depacketizer::new();
        feed(&mut dp, &shards, &[0, 2, 5], Instant::now());
        let evs = events(&mut dp);

        let au = only_frame(&evs);
        assert_eq!(&au.data[1..], &nal[..], "FEC 복구 결과가 원본과 다르다");
    }

    #[test]
    fn parity_count_is_exact_for_non_divisible_ratios() {
        // data=13, 25% → 올림해서 parity 4. 나누어떨어지지 않는 비율은 예전 와이어 규약(achieved%
        // 재역산)에서 송/수신 개수가 어긋나던 자리다 — RS 는 개수가 다르면 **에러 없이** 쓰레기를 낸다.
        let nal = make_nal(13 * 48 - 8, 21);
        let shards = packetize(&nal, false, 64, 2, 25);
        assert_eq!(shards.len(), 17, "13 data + 4 parity");

        let probe = NvVideoPacket::read_from_shard(&shards[0]).unwrap();
        assert_eq!(probe.data_shards(), 13);
        assert_eq!(probe.fec_percentage(), 25, "와이어에는 **요청한** 퍼센트가 그대로 실려야 한다");
        assert_eq!(probe.parity_shards(), 4, "수신 측 역산이 송신 개수와 정확히 같아야 한다");

        // parity 개수만큼(4장) 데이터를 잃어도 바이트 단위로 복구된다 — 어느 자리를 잃든.
        for drops in [&[0usize, 1, 4, 12][..], &[3, 7, 9, 11][..], &[9, 10, 11, 12][..]] {
            let mut dp = Depacketizer::new();
            feed(&mut dp, &shards, drops, Instant::now());
            assert_eq!(
                &only_frame(&events(&mut dp)).data[1..],
                &nal[..],
                "drops={drops:?} 에서 복구가 깨졌다"
            );
        }

        // parity 를 일부 잃어도 총 개수 계산은 그대로 4 — 남은 3장으로 데이터 2장을 살린다.
        let mut dp = Depacketizer::new();
        feed(&mut dp, &shards, &[3, 7, 16], Instant::now());
        assert_eq!(&only_frame(&events(&mut dp)).data[1..], &nal[..]);
    }

    #[test]
    fn unrecoverable_loss_drops_frame_and_signals() {
        // parity 3장인데 데이터 4장 손실 → 복구 불가.
        let nal = make_nal(6 * 48 - 8, 9);
        let shards = packetize(&nal, true, 64, 77, 50);
        let t0 = Instant::now();
        let mut dp = Depacketizer::new();
        feed(&mut dp, &shards, &[0, 1, 2, 3], t0);

        // 아직 시간 예산 안이라 붙들고 있어야 한다(지각 shard 를 받을 기회를 남긴다).
        assert!(events(&mut dp).is_empty(), "즉시 버리면 지각 shard 를 못 받는다");

        dp.expire(t0 + ASSEMBLY_BUDGET + Duration::from_millis(1));
        let evs = events(&mut dp);
        assert!(
            !evs.iter().any(|e| matches!(e, VideoEvent::Frame(_))),
            "복구 불가 프레임을 깨진 채로 내보냈다"
        );
        assert!(
            evs.contains(&VideoEvent::Lost { frame_index: 77 }),
            "손실 신호가 안 떴다: {evs:?}"
        );
        let st = fec_statuses(&evs);
        assert_eq!(st.len(), 1);
        assert_eq!(st[0].received_data_shards, 2);
        assert_eq!(st[0].received_parity_shards, 3);
        // 9장 중 4장 손실.
        let loss = st[0].loss_fraction().unwrap();
        assert!((loss - 4.0 / 9.0).abs() < 1e-6, "loss_fraction = {loss}");
    }

    #[test]
    fn fec_status_counts_match_observed_loss() {
        // 6 data + 3 parity. 데이터 2장(index 1,3)만 잃고 나머지 전부 수신 → 복구 성공 + 보고.
        let nal = make_nal(6 * 48 - 8, 31);
        let shards = packetize(&nal, false, 64, 5, 50);
        let t0 = Instant::now();
        let mut dp = Depacketizer::new();
        feed(&mut dp, &shards, &[1, 3], t0);
        // AU 는 복구 즉시 나가지만 통계는 프레임 마감 시점 — 지각 parity 까지 다 세고 낸다.
        dp.expire(t0 + ASSEMBLY_BUDGET + Duration::from_millis(1));
        let evs = events(&mut dp);

        assert_eq!(&only_frame(&evs).data[1..], &nal[..]);
        let st = fec_statuses(&evs);
        assert_eq!(st.len(), 1, "블록 하나짜리 프레임이면 상태도 하나");
        let s = st[0];
        assert_eq!(s.frame_index, 5);
        assert_eq!(s.total_data_shards, 6);
        assert_eq!(s.total_parity_shards, 3);
        assert_eq!(s.received_data_shards, 4);
        assert_eq!(s.received_parity_shards, 3);
        assert_eq!(s.block_index, 0);
        assert_eq!(s.block_count, 1);
        assert_eq!(s.fec_percentage, 50);
        // seq 0..=8, 1 과 3 손실. 최고 수신 seq = 8, 그 앞에 빠진 게 2개.
        assert_eq!(s.highest_received_seq, 8);
        assert_eq!(s.next_contiguous_seq, 1, "seq0 만 연속, seq1 이 빠져 멈춘다");
        assert_eq!(s.missing_before_highest, 2);
        let loss = s.loss_fraction().unwrap();
        assert!((loss - 2.0 / 9.0).abs() < 1e-6, "loss_fraction = {loss}");
    }

    #[test]
    fn lossless_frame_reports_no_fec_status() {
        // 데이터 전부 도착 + parity 는 끝내 미도착. 데이터 손실 0 이므로 마감해도 보고 금지.
        let nal = make_nal(6 * 48 - 8, 17);
        let shards = packetize(&nal, false, 64, 6, 50);
        let t0 = Instant::now();
        let mut dp = Depacketizer::new();
        feed(&mut dp, &shards, &[6, 7, 8], t0);
        dp.expire(t0 + ASSEMBLY_BUDGET + Duration::from_millis(1));
        let evs = events(&mut dp);
        assert_eq!(&only_frame(&evs).data[1..], &nal[..]);
        assert!(
            fec_statuses(&evs).is_empty(),
            "parity 미도착을 손실로 보고하면 호스트 비트레이트가 무너진다: {evs:?}"
        );
    }

    #[test]
    fn newer_complete_frame_evicts_older_incomplete_one() {
        let nal_a = make_nal(6 * 48 - 8, 41);
        let nal_b = make_nal(300, 42);
        let a = packetize(&nal_a, false, 64, 10, 50);
        let b = packetize(&nal_b, true, 1024, 11, 20);
        let now = Instant::now();

        let mut dp = Depacketizer::new();
        // 프레임 10 은 복구 불가(데이터 4장 손실)로 남겨두고,
        feed(&mut dp, &a, &[0, 1, 2, 3], now);
        // 프레임 11 이 온전히 도착 → 10 은 그 자리에서 폐기돼야 한다.
        feed(&mut dp, &b, &[], now);

        let evs = events(&mut dp);
        let lost_at = evs.iter().position(|e| *e == VideoEvent::Lost { frame_index: 10 });
        let frame_at = evs.iter().position(|e| matches!(e, VideoEvent::Frame(_)));
        assert!(lost_at.is_some(), "오래된 프레임 폐기 신호가 없다: {evs:?}");
        assert!(lost_at < frame_at, "이벤트가 frame_index 오름차순이어야 한다");
        assert_eq!(&only_frame(&evs).data[1..], &nal_b[..]);

        // 프레임 10 의 지각 shard 는 이제 무시돼야 한다(마감 워터마크).
        feed(&mut dp, &a, &[4, 5, 6, 7, 8], now);
        assert!(events(&mut dp).is_empty(), "마감한 프레임을 되살렸다");
    }

    #[test]
    fn inflight_bound_evicts_oldest() {
        let mut dp = Depacketizer::new();
        let nal = make_nal(6 * 48 - 8, 55);
        let now = Instant::now();
        // 각 프레임을 절반만 먹여 미완성으로 쌓는다(0,1 만 도착 → 6개 중 2개).
        for f in 1..=MAX_INFLIGHT_FRAMES as u32 {
            let s = packetize(&nal, false, 64, f, 50);
            feed(&mut dp, &s, &[2, 3, 4, 5, 6, 7, 8], now);
        }
        assert!(events(&mut dp).is_empty(), "아직 아무것도 마감 안 됐어야 한다");

        let s = packetize(&nal, false, 64, MAX_INFLIGHT_FRAMES as u32 + 1, 50);
        feed(&mut dp, &s, &[2, 3, 4, 5, 6, 7, 8], now);
        let evs = events(&mut dp);
        assert!(
            evs.contains(&VideoEvent::Lost { frame_index: 1 }),
            "가장 오래된 프레임이 밀려나야 한다: {evs:?}"
        );
    }

    #[test]
    fn stray_and_malformed_datagrams_are_ignored() {
        let mut dp = Depacketizer::new();
        let now = Instant::now();
        dp.push(b"PING", now); // 헤더보다 짧음
        dp.push(&[0u8; PAYLOAD_OFFSET], now); // payload 0바이트
        dp.push(&[0u8; PAYLOAD_OFFSET + 8], now); // version_flags 불일치
        assert!(events(&mut dp).is_empty());

        // 그 뒤 정상 프레임은 정상 처리돼야 한다.
        let nal = make_nal(200, 61);
        feed(&mut dp, &packetize(&nal, true, 1024, 1, 20), &[], now);
        assert_eq!(&only_frame(&events(&mut dp)).data[1..], &nal[..]);
    }

    #[test]
    fn duplicate_shards_do_not_corrupt_counts() {
        let nal = make_nal(6 * 48 - 8, 71);
        let shards = packetize(&nal, false, 64, 3, 50);
        let t0 = Instant::now();
        let mut dp = Depacketizer::new();
        // 데이터 1장 손실 + 나머지를 두 번씩 먹인다.
        feed(&mut dp, &shards, &[2], t0);
        feed(&mut dp, &shards, &[2], t0);
        dp.expire(t0 + ASSEMBLY_BUDGET + Duration::from_millis(1));
        let evs = events(&mut dp);
        assert_eq!(&only_frame(&evs).data[1..], &nal[..]);
        let s = fec_statuses(&evs);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].received_data_shards, 5, "중복이 카운트를 부풀렸다");
        assert_eq!(s[0].received_parity_shards, 3);
        let loss = s[0].loss_fraction().unwrap();
        assert!((loss - 1.0 / 9.0).abs() < 1e-6, "중복 때문에 손실률이 틀어졌다: {loss}");
    }

    #[tokio::test]
    async fn run_pings_host_and_delivers_frames() {
        let host = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let host_addr = host.local_addr().unwrap();

        let (au_tx, au_rx) = std::sync::mpsc::channel();
        let (fec_tx, _fec_rx) = tokio::sync::mpsc::unbounded_channel();
        let (lost_tx, _lost_rx) = tokio::sync::mpsc::unbounded_channel();
        let shutdown = Arc::new(AtomicBool::new(false));

        let task = tokio::spawn(run(
            "127.0.0.1:0".parse().unwrap(),
            host_addr,
            au_tx,
            fec_tx,
            lost_tx,
            shutdown.clone(),
        ));

        // 호스트 쪽에서 PING 을 받아 클라이언트 주소를 배운다 — 호스트 코드와 동일한 판정.
        let mut buf = [0u8; 64];
        let (n, client) = tokio::time::timeout(Duration::from_secs(3), host.recv_from(&mut buf))
            .await
            .expect("PING 이 안 왔다")
            .unwrap();
        assert_eq!(&buf[..n], PING_PAYLOAD, "호스트가 인식하는 payload 가 아니다");

        let nal = make_nal(3000, 77);
        for s in packetize(&nal, true, 1024, 1, 20) {
            host.send_to(&s, client).await.unwrap();
        }

        let au = tokio::task::spawn_blocking(move || au_rx.recv_timeout(Duration::from_secs(3)))
            .await
            .unwrap()
            .expect("AU 가 안 왔다");
        assert_eq!(au.data[0], 1);
        assert_eq!(&au.data[1..], &nal[..]);

        shutdown.store(true, Ordering::Relaxed);
        // shutdown 은 다음 이벤트(PING 틱 ≤ 500ms) 때 관측된다.
        tokio::time::timeout(PING_INTERVAL * 4, task).await.unwrap().unwrap().unwrap();
    }

    #[tokio::test]
    async fn run_repeats_ping_within_host_idle_window() {
        // 호스트는 3초 무소식이면 인코딩을 멈춘다 — 그 안에 최소 두 번은 더 와야 한다.
        let host = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let host_addr = host.local_addr().unwrap();
        let (au_tx, _au_rx) = std::sync::mpsc::channel();
        let (fec_tx, _f) = tokio::sync::mpsc::unbounded_channel();
        let (lost_tx, _l) = tokio::sync::mpsc::unbounded_channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(run(
            "127.0.0.1:0".parse().unwrap(),
            host_addr,
            au_tx,
            fec_tx,
            lost_tx,
            shutdown.clone(),
        ));

        let mut buf = [0u8; 64];
        let started = Instant::now();
        for i in 0..3 {
            let (n, _) = tokio::time::timeout(Duration::from_secs(3), host.recv_from(&mut buf))
                .await
                .unwrap_or_else(|_| panic!("PING #{i} 이 3초 안에 안 왔다"))
                .unwrap();
            assert_eq!(&buf[..n], PING_PAYLOAD);
        }
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "3회 PING 이 호스트 유휴 한계(3s)를 넘겼다"
        );

        shutdown.store(true, Ordering::Relaxed);
        let _ = tokio::time::timeout(PING_INTERVAL * 4, task).await;
    }

    /// RS parity 행이 **총 parity 개수**에 의존한다는 사실을 못박는다.
    ///
    /// 이게 아니라면 "관측한 shard 인덱스만큼 슬롯을 늘려 잡기" 같은 추정이 가능했을 것이다.
    /// 실제로는 개수가 하나만 달라도 parity 바이트가 통째로 바뀌고, 복구는 **에러 없이** 쓰레기를
    /// 낸다. 이 테스트가 깨지면 `parity_shard_count` 를 양쪽에서 똑같이 쓰는 계약을 느슨하게 해도
    /// 되는지 다시 판단해야 한다.
    #[test]
    fn rs_parity_rows_depend_on_total_parity_count() {
        let data: Vec<Vec<u8>> = (0..13u32)
            .map(|i| (0..48u32).map(|j| (i * 31 + j * 7 + 1) as u8).collect())
            .collect();
        let encode = |parity: usize| {
            let mut shards = data.clone();
            shards.extend(vec![vec![0u8; 48]; parity]);
            fec_rs::ReedSolomon::new(13, parity).unwrap().encode(&mut shards).unwrap();
            shards
        };
        let with3 = encode(3);
        let with4 = encode(4);

        assert_eq!(with3[..13], with4[..13], "데이터 shard 는 인코딩과 무관하게 그대로다");
        assert_ne!(with3[13], with4[13], "parity 개수가 달라도 같다면 추정이 허용됐을 것");
        assert_ne!(with3[14], with4[14]);
    }
}
