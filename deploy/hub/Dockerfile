# kmc-hub 컨테이너 (dokploy 등 Docker PaaS 호스팅용).
#
# 멀티스테이지: rust 로 정적에 가깝게 빌드 → 슬림 런타임.
# hub 는 순수 Rust + rusqlite(bundled, SQLite C 정적 컴파일)라 시스템 lib 의존이 없다.
#
# 빌드 컨텍스트 = 저장소 루트. (kmc-hub 와 kmc-proto 둘 다 필요.)
#   docker build -f deploy/hub/Dockerfile -t kmc-hub .
#
# dokploy: "Dockerfile" 앱으로 등록, Dockerfile Path = deploy/hub/Dockerfile,
#          Build Context = 저장소 루트(.). 도메인 붙이면 Traefik 이 HTTPS 종단.

# ---- build stage ----
FROM rust:1-bookworm AS build
WORKDIR /src
# 로컬 경로 의존(kmc-proto)까지 함께 복사.
COPY kmc-proto ./kmc-proto
COPY kmc-hub   ./kmc-hub
WORKDIR /src/kmc-hub
# rusqlite bundled 는 cc 로 SQLite 를 컴파일하므로 빌드 툴체인 필요(rust 이미지에 포함).
RUN cargo build --release --locked || cargo build --release
RUN strip target/release/kmc-hub || true

# ---- runtime stage ----
FROM debian:bookworm-slim AS runtime
# TLS 루트인증서(외부 호출용) + 정리. hub 자체는 평문 HTTP(8080), HTTPS 는 앞단(Traefik)이 종단.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
# 비루트 사용자.
RUN useradd -r -u 10001 -m kmc
WORKDIR /app
COPY --from=build /src/kmc-hub/target/release/kmc-hub /usr/local/bin/kmc-hub

# DB 는 볼륨에 영속. dokploy 에서 /data 를 볼륨 마운트.
ENV KMC_HUB_DB=/data/hub.db
ENV KMC_HUB_ADDR=0.0.0.0:8080
RUN mkdir -p /data && chown kmc:kmc /data
VOLUME ["/data"]
USER kmc
EXPOSE 8080

# admin 계정은 최초 1회 `docker exec <c> kmc-hub admin add --username .. --password ..` 로 생성.
# enroll 은 env 로 구성: KMC_ENROLL_SECRET / KMC_ENROLL_AUTHKEY / KMC_ENROLL_HUB_URL / KMC_RELEASE_URL.
CMD ["kmc-hub", "serve"]
