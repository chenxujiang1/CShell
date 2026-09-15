#!/usr/bin/env bash
set -euo pipefail

if ! command -v sshd >/dev/null 2>&1; then
  sudo apt-get update
  sudo apt-get install --no-install-recommends -y openssh-server
fi

interop_dir="$(mktemp -d)"
sshd_pid=""
agent_pid=""
interop_user="cshellinterop"
interop_user_created=0

cleanup() {
  if [[ -n "${sshd_pid}" ]]; then
    sudo kill "${sshd_pid}" >/dev/null 2>&1 || true
  fi
  if [[ -n "${agent_pid}" ]]; then
    kill "${agent_pid}" >/dev/null 2>&1 || true
  fi
  if [[ "${interop_user_created}" -eq 1 ]]; then
    sudo userdel --remove "${interop_user}" >/dev/null 2>&1 || true
  fi
  rm -rf "${interop_dir}"
}
trap cleanup EXIT

if id "${interop_user}" >/dev/null 2>&1; then
  echo "reserved OpenSSH interop user already exists: ${interop_user}" >&2
  exit 1
fi
sudo useradd --create-home --shell /bin/sh "${interop_user}"
# An empty password keeps the account unlocked while password authentication
# remains disabled in the isolated sshd configuration below.
sudo passwd --delete "${interop_user}" >/dev/null
interop_user_created=1

ssh-keygen -q -t ed25519 -N '' -f "${interop_dir}/host_key"
ssh-keygen -q -t ed25519 -N '' -f "${interop_dir}/public_key"
ssh-keygen -q -t ed25519 -N '' -f "${interop_dir}/certificate_key"
ssh-keygen -q -t ed25519 -N '' -f "${interop_dir}/user_ca"
ssh-keygen -q -s "${interop_dir}/user_ca" -I cshell-ci-user -n "${interop_user}" -V -1m:+10m "${interop_dir}/certificate_key.pub"
cp "${interop_dir}/public_key.pub" "${interop_dir}/authorized_keys"
chmod 600 "${interop_dir}/host_key" "${interop_dir}/public_key" "${interop_dir}/certificate_key" "${interop_dir}/user_ca" "${interop_dir}/authorized_keys"
chmod 711 "${interop_dir}"
sudo chown "${interop_user}:${interop_user}" "${interop_dir}/authorized_keys"

port="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"
cat >"${interop_dir}/sshd_config" <<EOF
Port ${port}
ListenAddress 127.0.0.1
HostKey ${interop_dir}/host_key
PidFile ${interop_dir}/sshd.pid
AuthorizedKeysFile ${interop_dir}/authorized_keys
TrustedUserCAKeys ${interop_dir}/user_ca.pub
PasswordAuthentication no
KbdInteractiveAuthentication no
ChallengeResponseAuthentication no
UsePAM no
StrictModes no
PermitTTY yes
AllowUsers ${interop_user}
LogLevel VERBOSE
EOF

sudo mkdir -p /run/sshd
sudo /usr/sbin/sshd -D -e -f "${interop_dir}/sshd_config" 2>"${interop_dir}/sshd.log" &
sshd_pid="$!"
sshd_ready=0
for _ in $(seq 1 100); do
  if ssh-keyscan -T 1 -p "${port}" 127.0.0.1 >/dev/null 2>&1; then
    sshd_ready=1
    break
  fi
  sleep 0.05
done
if [[ "${sshd_ready}" -ne 1 ]] || ! kill -0 "${sshd_pid}" >/dev/null 2>&1; then
  cat "${interop_dir}/sshd.log"
  exit 1
fi

eval "$(ssh-agent -s)"
agent_pid="${SSH_AGENT_PID}"
ssh-add "${interop_dir}/certificate_key"

export CSHELL_OPENSSH_INTEROP=1
export CSHELL_OPENSSH_ADDRESS="127.0.0.1:${port}"
export CSHELL_OPENSSH_USERNAME="${interop_user}"
export CSHELL_OPENSSH_HOST_FINGERPRINT
CSHELL_OPENSSH_HOST_FINGERPRINT="$(ssh-keygen -q -l -E sha256 -f "${interop_dir}/host_key.pub" | awk '{print $2}')"
export CSHELL_OPENSSH_PUBLIC_KEY="${interop_dir}/public_key"
export CSHELL_OPENSSH_CERTIFICATE_KEY="${interop_dir}/certificate_key"
export CSHELL_OPENSSH_CERTIFICATE="${interop_dir}/certificate_key-cert.pub"

/usr/sbin/sshd -V 2>&1 || true
cargo test -p cshell-ssh --test openssh_interop --all-features --locked -- --test-threads=1
