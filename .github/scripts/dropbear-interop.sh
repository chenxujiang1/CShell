#!/usr/bin/env bash
set -euo pipefail

if ! command -v dropbear >/dev/null 2>&1; then
  sudo apt-get update
  sudo apt-get install --no-install-recommends -y dropbear-bin
fi

interop_dir="$(mktemp -d)"
server_pid=""
interop_user="cshelldropbear"
interop_user_created=0

cleanup() {
  if [[ -n "${server_pid}" ]]; then
    sudo kill "${server_pid}" >/dev/null 2>&1 || true
  fi
  if [[ "${interop_user_created}" -eq 1 ]]; then
    sudo userdel --remove "${interop_user}" >/dev/null 2>&1 || true
  fi
  rm -rf "${interop_dir}"
}
trap cleanup EXIT

if id "${interop_user}" >/dev/null 2>&1; then
  echo "reserved Dropbear interop user already exists: ${interop_user}" >&2
  exit 1
fi
sudo useradd --create-home --shell /bin/sh "${interop_user}"
sudo passwd --delete "${interop_user}" >/dev/null
interop_user_created=1

ssh-keygen -q -t ed25519 -N '' -f "${interop_dir}/client_key"
dropbearkey -t ed25519 -f "${interop_dir}/host_key" >/dev/null
sudo install -d -m 700 -o "${interop_user}" -g "${interop_user}" "/home/${interop_user}/.ssh"
sudo install -m 600 -o "${interop_user}" -g "${interop_user}" \
  "${interop_dir}/client_key.pub" "/home/${interop_user}/.ssh/authorized_keys"

port="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"
sudo /usr/sbin/dropbear -F -E -m -s -w -p "127.0.0.1:${port}" \
  -P "${interop_dir}/dropbear.pid" -r "${interop_dir}/host_key" \
  2>"${interop_dir}/dropbear.log" &
server_pid="$!"

server_ready=0
for _ in $(seq 1 100); do
  if ssh-keyscan -T 1 -p "${port}" 127.0.0.1 >/dev/null 2>&1; then
    server_ready=1
    break
  fi
  sleep 0.05
done
if [[ "${server_ready}" -ne 1 ]] || ! kill -0 "${server_pid}" >/dev/null 2>&1; then
  cat "${interop_dir}/dropbear.log"
  exit 1
fi

export CSHELL_BASIC_SSH_INTEROP=1
export CSHELL_BASIC_SSH_ADDRESS="127.0.0.1:${port}"
export CSHELL_BASIC_SSH_USERNAME="${interop_user}"
export CSHELL_BASIC_SSH_HOST_FINGERPRINT
CSHELL_BASIC_SSH_HOST_FINGERPRINT="$(dropbearkey -y -f "${interop_dir}/host_key" 2>&1 | awk '/^Fingerprint:/{print $2}')"
export CSHELL_BASIC_SSH_PRIVATE_KEY="${interop_dir}/client_key"

dropbear -V
cargo test -p cshell-ssh --test basic_ssh_interop --all-features --locked -- --test-threads=1
