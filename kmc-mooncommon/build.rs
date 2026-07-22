use std::env;
use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

    // 래퍼 CMakeLists로 mbedcrypto + moonlight-common-c를 정적 빌드.
    // MSVC /WX(경고를 에러로)를 /WX-로 무력화 — upstream이 C4267 등 경고를 에러로 처리해
    // 우리 툴체인에서 빌드가 깨지는 것을 막는다 (vendored 파일은 수정하지 않음).
    let mut cfg = cmake::Config::new(&manifest);
    cfg.define("BUILD_SHARED_LIBS", "OFF")
        .define("USE_MBEDTLS", "ON")
        .build_target("moonlight-common-c");
    // Rust는 릴리스 CRT(/MD)로 링크하므로 cmake도 Release 프로필로 빌드해
    // Debug CRT(/MDd) 불일치(__imp__CrtDbgReportW 미해결)를 피한다.
    cfg.profile("Release");
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        cfg.cflag("/WX-");
    }
    let dst = cfg.build();

    // cmake 빌드 산출물 경로. (build_target을 쓰면 install 대신 build 디렉토리에 위치)
    let build = dst.join("build");
    // moonlight-common-c 정적 라이브러리.
    println!("cargo:rustc-link-search=native={}", build.display());
    println!("cargo:rustc-link-search=native={}", build.join("enet").display());
    println!(
        "cargo:rustc-link-search=native={}",
        build.join("vendor").display()
    );
    println!(
        "cargo:rustc-link-search=native={}",
        build.join("vendor/enet").display()
    );
    // mbedcrypto 위치 (mbedtls 빌드 트리).
    println!(
        "cargo:rustc-link-search=native={}",
        build.join("vendor/mbedtls/library").display()
    );

    // Release/Debug 하위 디렉토리도 탐색 (MSVC 멀티컨피그).
    for sub in ["Release", "Debug"] {
        println!("cargo:rustc-link-search=native={}", build.join(sub).display());
        println!(
            "cargo:rustc-link-search=native={}",
            build.join("vendor").join(sub).display()
        );
        println!(
            "cargo:rustc-link-search=native={}",
            build.join("vendor/enet").join(sub).display()
        );
        println!(
            "cargo:rustc-link-search=native={}",
            build.join("vendor/mbedtls/library").join(sub).display()
        );
    }

    println!("cargo:rustc-link-lib=static=moonlight-common-c");
    println!("cargo:rustc-link-lib=static=enet");
    println!("cargo:rustc-link-lib=static=mbedcrypto");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rustc-link-lib=dylib=ws2_32");
        println!("cargo:rustc-link-lib=dylib=winmm");
        // mbedtls entropy_poll이 BCryptGenRandom을 쓴다.
        println!("cargo:rustc-link-lib=dylib=bcrypt");
    }

    // bindgen: Limelight.h 공개 API 바인딩 생성.
    let header = manifest.join("vendor/src/Limelight.h");
    println!("cargo:rerun-if-changed={}", header.display());
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=CMakeLists.txt");

    let bindings = bindgen::Builder::default()
        .header(header.to_string_lossy())
        .allowlist_function("Li.*")
        .allowlist_type("_?(STREAM_CONFIGURATION|SERVER_INFORMATION|DECODER_RENDERER_CALLBACKS|AUDIO_RENDERER_CALLBACKS|CONNECTION_LISTENER_CALLBACKS|DECODE_UNIT|LENTRY|OPUS_MULTISTREAM_CONFIGURATION).*")
        .allowlist_var("(VIDEO_FORMAT|CAPABILITY|BUFFER_TYPE|FRAME_TYPE|ML_ERROR|STAGE|ENCFLG|AUDIO_CONFIGURATION|DR_|CONN_STATUS|STREAM_CFG|COLORSPACE|COLOR_RANGE)_.*")
        .generate()
        .expect("bindgen: failed to generate Limelight bindings");

    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out.join("bindings.rs"))
        .expect("bindgen: failed to write bindings");
}
