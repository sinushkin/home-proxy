# Чистая Debian/Ubuntu с systemd и sshd, как свежий VPS: root + пароль, больше ничего.
# Для клиентов дополнительно обычный пользователь `user` с sudo без пароля (CLIENT_USER=1).
ARG BASE=debian:12
FROM ${BASE}
ARG APT_PROXY=
ARG ROOT_PASSWORD=rootpass
ARG CLIENT_USER=0
ENV DEBIAN_FRONTEND=noninteractive
# Прокси задаём флагами только на время сборки: в образе его не остаётся.
RUN set -eu; P=""; [ -z "$APT_PROXY" ] || P="-o Acquire::http::Proxy=$APT_PROXY"; \
    apt-get $P update; \
    apt-get $P install -y --no-install-recommends systemd systemd-sysv openssh-server sudo \
        iproute2 iptables iputils-ping curl ca-certificates procps dbus; \
    rm -rf /var/lib/apt/lists/*; \
    echo "root:${ROOT_PASSWORD}" | chpasswd; \
    printf 'PermitRootLogin yes\nPasswordAuthentication yes\n' >> /etc/ssh/sshd_config; \
    if [ "$CLIENT_USER" = 1 ]; then \
        useradd -m -s /bin/bash testuser; echo "testuser:${ROOT_PASSWORD}" | chpasswd; \
        echo 'testuser ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/testuser; \
    fi; \
    systemctl enable ssh; \
    rm -f /etc/machine-id /var/lib/dbus/machine-id
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
