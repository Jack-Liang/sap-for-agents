# Dockerfile for sap-for-agents
#
# 策略：runtime 镜像 **不含** SAP SDK（避免版权问题，镜像可自由分发）。
# SDK 在运行时通过卷挂载注入：docker run -v /host/sap-sdk-linux:/app/nwrfcsdk ...
#
# 前置（构建期）：
#   需把 Linux 版 SAP NWRFC SDK 放到 nwrfcsdk/lib/linux-x86_64/ 下
#   （含 libsapnwrfc.so 等），build.rs 会据此链接。
#   详见 nwrfcsdk/README.md。
#
# 构建：docker build -t sap-for-agents .
# 运行：见下方 EXAMPLE，或 README §9

# ============ 构建阶段 ============
FROM docker.m.daocloud.io/library/rust:1.93-slim-bookworm AS builder

# 编译期依赖：libssl 链接（SAP SDK 依赖 openssl）
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# 先拷依赖清单，利用 Docker 层缓存
COPY Cargo.toml Cargo.lock ./
# 创建占位 src 目录，先编译依赖（缓存层）
RUN mkdir src && echo "fn main() {}" > src/main.rs && \
    cargo build --release || true

# 拷真实源码 + SAP SDK（Linux 版，构建期链接用）+ build.rs
COPY src/ ./src/
COPY nwrfcsdk/ ./nwrfcsdk/
COPY build.rs ./

# 重新编译真实二进制
RUN touch src/main.rs && cargo build --release

# ============ 运行阶段 ============
FROM debian:bookworm-slim AS runtime

# 运行期依赖：libssl + ca-certificates（HTTPS）+ curl（docker-compose 健康检查）
# 注意：不安装 SAP SDK，由运行时挂载提供
RUN apt-get update && apt-get install -y --no-install-recommends \
    libssl3 \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# 只拷贝编译产物，不拷贝 nwrfcsdk/
COPY --from=builder /app/target/release/sap_for_agents /app/sap_for_agents

# 挂载点：宿主机 SAP SDK 在运行时挂到此处
# 期望目录结构：/app/nwrfcsdk/lib/linux-x86_64/libsapnwrfc.so (+ libsapucum.so)
RUN mkdir -p /app/nwrfcsdk/lib

# 让动态链接器找到挂载进来的 libsapnwrfc.so
ENV LD_LIBRARY_PATH=/app/nwrfcsdk/lib/linux-x86_64

# 日志按级别过滤（受 RUST_LOG 控制）
ENV RUST_LOG=info

# 暴露默认监听端口
EXPOSE 3000

# SAP 连接参数通过 -e 在运行时注入，不烘焙进镜像
# 示例（SDK 通过 -v 挂载）：
#   docker run --rm -p 3000:3000 \
#     -v /opt/sap/nwrfcsdk-linux:/app/nwrfcsdk \
#     -e SAP_ASHOST=sap.example.com \
#     -e SAP_SYSNR=00 \
#     -e SAP_CLIENT=100 \
#     -e SAP_USER=DEVELOPER \
#     -e SAP_PASSWD=secret \
#     sap-for-agents
CMD ["/app/sap_for_agents"]
