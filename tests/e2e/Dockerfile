# sshd on Arch for the anvil e2e: `key` signs in with the authorized key
# passed as AUTHORIZED_KEY at start, `pw` only with the password `pw-secret`.
# inetutils provides `hostname`, the config placeholder step `apply` runs;
# diffutils provides `cmp`, which a `[[files]]` step uses to print `unchanged`.
# sudo: `key` has passwordless sudo (the `[packages]` step's `sudo -n`), `pw`
# needs its password (typed by anvil from a secret).
# Two stages: `sshd` runs sshd as PID 1; `systemd` boots systemd as PID 1
# with sshd as a unit, `key` as its only login, and anvil-e2e.service (not
# enabled) for the services checks.
# BASE defaults to the official amd64 image; tests/e2e.sh passes the pinned
# digest and picks an Arch Linux ARM build on arm64 hosts, where sshd's seccomp
# sandbox cannot start under emulation.
ARG BASE=archlinux@sha256:52dfbd5afb23a220d5ee92dafcb4c788d96c026ed04179890c23adef4c897fa0
FROM ${BASE} AS sshd

# pacman's own seccomp sandbox has the same problem under emulation.
RUN sed -i 's/^#\?DisableSandbox/DisableSandbox/' /etc/pacman.conf \
    && (grep -q '^DisableSandbox' /etc/pacman.conf || echo 'DisableSandbox' >> /etc/pacman.conf) \
    && pacman -Sy --noconfirm --needed filesystem openssh inetutils diffutils sudo \
    && pacman -Scc --noconfirm \
    && ssh-keygen -A \
    && useradd -m -s /bin/bash -p '*' key \
    && useradd -m -s /bin/bash pw \
    && echo 'pw:pw-secret' | chpasswd \
    && echo 'key ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/key \
    && chmod 0440 /etc/sudoers.d/key \
    && visudo -cf /etc/sudoers.d/key \
    && echo 'pw ALL=(ALL) ALL' > /etc/sudoers.d/pw \
    && chmod 0440 /etc/sudoers.d/pw \
    && visudo -cf /etc/sudoers.d/pw

COPY sshd_config /etc/ssh/sshd_config

EXPOSE 22

CMD ["sh", "-c", "install -d -m 700 -o key -g key /home/key/.ssh \
  && printf '%s\\n' \"$AUTHORIZED_KEY\" > /home/key/.ssh/authorized_keys \
  && chown key:key /home/key/.ssh/authorized_keys \
  && chmod 600 /home/key/.ssh/authorized_keys \
  && exec /usr/sbin/sshd -D -e"]

FROM sshd AS systemd

# Both Arch bases ship systemd; installing it only when missing keeps pacman
# offline. The key arrives through `docker exec` after boot: PID 1 is systemd,
# so the sshd stage's AUTHORIZED_KEY step never runs. The container runs
# --privileged, which shares the host's /dev and a writable /proc/sys: udev,
# the gettys, and the units that write kernel settings (sysctl, modules,
# binfmt) are masked so they leave the host's devices, ttys and kernel alone,
# as are the units that read or write the host's pstore, EFI random seed and
# TPM. Only `key` signs in here: the `pw` user and its sudo rule are removed
# and sshd takes keys only.
RUN (pacman -Q systemd >/dev/null 2>&1 \
      || (pacman -Sy --noconfirm systemd && pacman -Scc --noconfirm)) \
    && userdel -r pw \
    && rm /etc/sudoers.d/pw \
    && sed -i 's/^PasswordAuthentication yes$/PasswordAuthentication no/' /etc/ssh/sshd_config \
    && grep -qx 'PasswordAuthentication no' /etc/ssh/sshd_config \
    && systemctl enable sshd.service \
    && systemctl mask systemd-firstboot.service \
      systemd-udevd.service systemd-udev-trigger.service getty.target \
      systemd-sysctl.service systemd-modules-load.service systemd-binfmt.service \
      proc-sys-fs-binfmt_misc.automount systemd-pstore.service \
      systemd-boot-random-seed.service systemd-tpm2-setup.service \
      systemd-tpm2-setup-early.service \
    && printf '%s\n' '[Unit]' 'Description=anvil e2e fixture' '' '[Service]' 'Type=simple' \
      'ExecStart=/usr/bin/sleep infinity' '' '[Install]' 'WantedBy=multi-user.target' \
      > /etc/systemd/system/anvil-e2e.service

STOPSIGNAL SIGRTMIN+3

CMD ["/usr/lib/systemd/systemd"]
