cmake_minimum_required(VERSION 3.15)
project(kmc_mooncommon_wrapper LANGUAGES C)

# mbedcrypto만 필요 (moonlight-common-c의 crypto 백엔드).
# mbedtls 전체 빌드 대신 crypto 라이브러리 + 프로그램/테스트 비활성화.
set(ENABLE_PROGRAMS OFF CACHE BOOL "" FORCE)
set(ENABLE_TESTING OFF CACHE BOOL "" FORCE)
set(MBEDTLS_FATAL_WARNINGS OFF CACHE BOOL "" FORCE)
add_subdirectory(vendor/mbedtls)

# moonlight-common-c: USE_MBEDTLS 경로에서 mbedcrypto 타겟을 링크한다.
set(USE_MBEDTLS ON CACHE BOOL "" FORCE)
set(BUILD_SHARED_LIBS OFF CACHE BOOL "" FORCE)
add_subdirectory(vendor)
