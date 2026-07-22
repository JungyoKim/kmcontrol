//! moonlight-common-c Rust FFI 바인딩 (GameStream 클라이언트 프로토콜).
//!
//! build.rs가 cmake로 mbedcrypto + moonlight-common-c를 정적 빌드하고,
//! bindgen으로 Limelight.h 공개 API 바인딩을 생성한다.
//!
//! 안전하지 않은 원시 FFI. 상위 크레이트가 안전한 래퍼를 제공한다.

#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(dead_code)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
