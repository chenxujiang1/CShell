#!/usr/bin/env bash
set -euo pipefail

export DEBIAN_FRONTEND=noninteractive
apt-get update >/dev/null
apt-get install --no-install-recommends -y openssh-server >/dev/null

interop_user="cshellinterop"
useradd --create-home --shell /bin/sh "${interop_user}"
passwd --delete "${interop_user}" >/dev/null
mkdir -p /run/sshd

/usr/sbin/sshd -V 2>&1
exec /usr/sbin/sshd -D -e -f /interop/sshd_config
