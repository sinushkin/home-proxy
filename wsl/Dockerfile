# Лёгкий образ Alpine для WSL2: WireGuard + NAT + server (дыры -> WireGuard).
# Сборка из корня репозитория: wsl/build.sh (или docker build -f wsl/Dockerfile .).
FROM rust:alpine AS build
RUN apk add --no-cache musl-dev protoc
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY hp-backend hp-backend
COPY server server
COPY router router
RUN cargo build --release -p server && strip target/release/server

FROM alpine:3.20
RUN apk add --no-cache wireguard-tools iptables iproute2 \
    && rm -rf /var/cache/apk/* /etc/motd
COPY --from=build /src/target/release/server /usr/local/bin/homeproxy-server
COPY wsl/rootfs/ /
RUN chmod +x /usr/local/bin/homeproxy-start /usr/local/bin/homeproxy-stop \
    && mkdir -p /etc/homeproxy /etc/wireguard
