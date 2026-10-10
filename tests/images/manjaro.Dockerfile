# Чистый Manjaro с systemd и sshd: root + пароль и пользователь testuser с sudo без пароля.
FROM manjarolinux/base
ARG ROOT_PASSWORD=rootpass
RUN pacman -Syu --noconfirm --needed openssh sudo iproute2 iptables-nft iputils inetutils curl && \
    pacman -Scc --noconfirm && \
    echo "root:${ROOT_PASSWORD}" | chpasswd && \
    printf 'PermitRootLogin yes\nPasswordAuthentication yes\n' >> /etc/ssh/sshd_config && \
    useradd -m -s /bin/bash testuser && echo "testuser:${ROOT_PASSWORD}" | chpasswd && \
    echo 'testuser ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/testuser && \
    ssh-keygen -A && systemctl enable sshd && \
    rm -f /etc/machine-id
STOPSIGNAL SIGRTMIN+3
CMD ["/usr/lib/systemd/systemd"]
