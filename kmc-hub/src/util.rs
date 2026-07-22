use rand::RngCore;

/// n바이트 랜덤을 hex 문자열로 (길이 2n).
pub fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut buf);
    let mut s = String::with_capacity(n * 2);
    for b in buf {
        s.push_str(&format!("{:02x}", b));
    }
    s
}
