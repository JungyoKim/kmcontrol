//! R3d 검증: FFmpeg h264_qsv 인코더를 열고 합성 NV12 프레임을 인코딩해 패킷 출력을 확인.
//! Intel QSV 하드웨어 인코딩이 실제로 동작하는지 격리 검증.

extern crate ffmpeg_next as ffmpeg;

use ffmpeg::{codec, encoder, format, frame, Dictionary, Rational};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    ffmpeg::init()?;

    let (w, h, fps) = (1920u32, 1080u32, 60i32);

    // h264_qsv 코덱 찾기.
    let codec = encoder::find_by_name("h264_qsv").ok_or("h264_qsv encoder not found")?;
    println!("found encoder: {}", codec.name());

    let ctx = codec::context::Context::new_with_codec(codec);
    let mut enc = ctx.encoder().video()?;
    enc.set_width(w);
    enc.set_height(h);
    enc.set_format(format::Pixel::NV12);
    enc.set_time_base(Rational(1, fps));
    enc.set_frame_rate(Some(Rational(fps, 1)));
    enc.set_bit_rate(10_000_000);
    // 저지연 설정.
    let mut opts = Dictionary::new();
    opts.set("preset", "veryfast");
    opts.set("forced_idr", "1");
    opts.set("low_delay_brc", "1");
    opts.set("async_depth", "1");

    let mut encoder = enc.open_with(opts)?;
    println!("h264_qsv opened: {}x{} @ {}fps", w, h, fps);

    // 합성 NV12 프레임 (회색 + 그라디언트).
    let mut total_packets = 0usize;
    let mut total_bytes = 0usize;
    for i in 0..30i64 {
        let mut vframe = frame::Video::new(format::Pixel::NV12, w, h);
        // Y 평면.
        {
            let stride = vframe.stride(0);
            let data = vframe.data_mut(0);
            for y in 0..h as usize {
                for x in 0..w as usize {
                    data[y * stride + x] = ((x + i as usize * 3) & 0xff) as u8;
                }
            }
        }
        // UV 평면 (중립 회색 128).
        {
            let stride = vframe.stride(1);
            let data = vframe.data_mut(1);
            for y in 0..(h as usize / 2) {
                for x in 0..w as usize {
                    data[y * stride + x] = 128;
                }
            }
        }
        vframe.set_pts(Some(i));
        encoder.send_frame(&vframe)?;
        let mut pkt = ffmpeg::Packet::empty();
        while encoder.receive_packet(&mut pkt).is_ok() {
            total_packets += 1;
            total_bytes += pkt.size();
            let key = pkt.is_key();
            if total_packets <= 3 {
                println!("packet {}: {} bytes, key={}", total_packets, pkt.size(), key);
            }
        }
    }
    // flush.
    encoder.send_eof()?;
    let mut pkt = ffmpeg::Packet::empty();
    while encoder.receive_packet(&mut pkt).is_ok() {
        total_packets += 1;
        total_bytes += pkt.size();
    }

    println!("QSV encode complete: {} packets, {} bytes", total_packets, total_bytes);
    if total_packets == 0 {
        return Err("QSV produced no packets".into());
    }
    Ok(())
}
