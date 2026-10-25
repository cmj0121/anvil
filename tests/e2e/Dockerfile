# sshd on Arch for the anvil e2e: `key` signs in with the authorized key
# passed as AUTHORIZED_KEY at start, `pw` only with the password `pw-secret`.
# inetutils provides `hostname`, the config placeholder step `apply` runs.
# sudo: `key` has passwordless sudo (the `[packages]` step's `sudo -n`), `pw`
# is not in sudoers.
# BASE defaults to the official amd64 image; tests/e2e.sh passes the pinned
# digest and picks an Arch Linux ARM build on arm64 hosts, where sshd's seccomp
# sandbox cannot start under emulation.
ARG BASE=archlinux@sha256:52dfbd5afb23a220d5ee92dafcb4c788d96c026ed04179890c23adef4c897fa0
FROM ${BASE}

# pacman's own seccomp sandbox has the same problem under emulation.
RUN sed -i 's/^#\?DisableSandbox/DisableSandbox/' /etc/pacman.conf \
    && (grep -q '^DisableSandbox' /etc/pacman.conf || echo 'DisableSandbox' >> /etc/pacman.conf) \
    && pacman -Sy --noconfirm --needed filesystem openssh inetutils sudo \
    && pacman -Scc --noconfirm \
    && ssh-keygen -A \
    && useradd -m -s /bin/bash -p '*' key \
    && useradd -m -s /bin/bash pw \
    && echo 'pw:pw-secret' | chpasswd \
    && echo 'key ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/key \
    && chmod 0440 /etc/sudoers.d/key \
    && visudo -cf /etc/sudoers.d/key

COPY sshd_config /etc/ssh/sshd_config

EXPOSE 22

CMD ["sh", "-c", "install -d -m 700 -o key -g key /home/key/.ssh \
  && printf '%s\\n' \"$AUTHORIZED_KEY\" > /home/key/.ssh/authorized_keys \
  && chown key:key /home/key/.ssh/authorized_keys \
  && chmod 600 /home/key/.ssh/authorized_keys \
  && exec /usr/sbin/sshd -D -e"]
