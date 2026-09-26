# Лёгкий образ Alpine для WSL2: hp-server (дыры -> TUN) + NAT.
# Сборка из корня репозитория: wsl/build.sh (или docker build -f wsl/Dockerfile .).
FROM rust:alpine AS build
RUN apk add --no-cache musl-dev protoc
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY hp-backend hp-backend
COPY hp-server hp-server
COPY hp-router hp-router
COPY vps-server vps-server
COPY vps-client vps-client
RUN cargo build --release -p hp-server && strip target/release/hp-server

FROM alpine:3.20
RUN apk add --no-cache iptables iproute2 \
    && rm -rf /var/cache/apk/* /etc/motd
COPY --from=build /src/target/release/hp-server /usr/local/bin/homeproxy-server
COPY wsl/rootfs/ /
RUN chmod +x /usr/local/bin/homeproxy-start /usr/local/bin/homeproxy-stop \
    && mkdir -p /etc/homeproxy
