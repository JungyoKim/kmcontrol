//! 헤드리스 입력 스모크: 세션 수립 후 절대 마우스 위치를 두 지점으로 전송하고
//! 실제 시스템 커서가 이동했는지 GetCursorPos로 확인한다.
//! 전체 경로 증명: client FFI(LiSendMousePositionEvent) → control 암호화 → 호스트 복호 → 파싱 → SendInput.
use kmc_admin_lib::stream::StreamState;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct Point {
    x: i32,
    y: i32,
}
extern "system" {
    fn GetCursorPos(p: *mut Point) -> i32;
}
fn cursor() -> Point {
    let mut p = Point::default();
    unsafe {
        GetCursorPos(&mut p);
    }
    p
}

fn main() -> anyhow::Result<()> {
    let st = StreamState::default();
    st.start("127.0.0.1", 1920, 1080, 60, None)?;
    // 연결 + moonlight 입력 스트림 초기화 대기.
    std::thread::sleep(std::time::Duration::from_millis(2000));

    let (w, h) = (1920i16, 1080i16);
    // 좌상단 근처로.
    kmc_moonclient::send_mouse_position(w / 5, h / 5, w, h);
    std::thread::sleep(std::time::Duration::from_millis(500));
    let p1 = cursor();

    // 우하단 근처로.
    kmc_moonclient::send_mouse_position((w * 4) / 5, (h * 4) / 5, w, h);
    std::thread::sleep(std::time::Duration::from_millis(500));
    let p2 = cursor();

    println!("cursor p1={:?} p2={:?}", p1, p2);
    let moved = p1.x != p2.x || p1.y != p2.y;
    let correct_dir = p2.x > p1.x && p2.y > p1.y;
    println!("→ cursor moved = {moved}; direction (p2 right/below p1) = {correct_dir}");

    st.stop();
    Ok(())
}
