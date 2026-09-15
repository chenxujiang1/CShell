#!/usr/bin/env bash
set -euo pipefail

image="${1:?an OpenSSH container image is required}"
cargo_command="${2:-cargo}"
case "${image}" in
  ubuntu:22.04) ;;
  *)
    echo "unsupported OpenSSH interop image: ${image}" >&2
    exit 2
    ;;
esac

interop_dir="$(mktemp -d)"
container_name="cshell-openssh-${GITHUB_RUN_ID:-local}-${RANDOM}"
agent_pid=""

cleanup() {
  docker rm --force "${container_name}" >/dev/null 2>&1 || true
  if [[ -n "${agent_pid}" ]]; then
    kill "${agent_pid}" >/dev/null 2>&1 || true
  fi
  rm -rf "${interop_dir}"
}
trap cleanup EXIT

report_failure() {
  local line="$1"
  local status="$2"
  trap - ERR
  docker logs "${container_name}" 2>&1 || true
  echo "::error title=OpenSSH historical interoperability::script line ${line} exited with status ${status}"
  exit "${status}"
}
trap 'report_failure "${LINENO}" "$?"' ERR

ssh-keygen -q -t ed25519 -N '' -f "${interop_dir}/host_key"
ssh-keygen -q -t ed25519 -N '' -f "${interop_dir}/public_key"
ssh-keygen -q -t ed25519 -N '' -f "${interop_dir}/certificate_key"
ssh-keygen -q -t ed25519 -N '' -f "${interop_dir}/user_ca"
ssh-keygen -q -s "${interop_dir}/user_ca" -I cshell-ci-user -n cshellinterop \
  -V -1m:+10m "${interop_dir}/certificate_key.pub"
cp "${interop_dir}/public_key.pub" "${interop_dir}/authorized_keys"
chmod 600 "${interop_dir}/host_key" "${interop_dir}/public_key" \
  "${interop_dir}/certificate_key" "${interop_dir}/user_ca"
chmod 644 "${interop_dir}/authorized_keys" "${interop_dir}/user_ca.pub"
chmod 755 "${interop_dir}"

cat >"${interop_dir}/sshd_config" <<EOF
Port 2222
ListenAddress 0.0.0.0
HostKey /interop/host_key
PidFile /tmp/sshd.pid
AuthorizedKeysFile /interop/authorized_keys
TrustedUserCAKeys /interop/user_ca.pub
PasswordAuthentication no
KbdInteractiveAuthentication no
ChallengeResponseAuthentication no
UsePAM no
StrictModes no
PermitTTY yes
AllowUsers cshellinterop
LogLevel VERBOSE
EOF

port="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"
docker run --detach --name "${container_name}" \
  --publish "127.0.0.1:${port}:2222" \
  --volume "${interop_dir}:/interop:ro" \
  --volume "${PWD}/.github/scripts/openssh-container-entrypoint.sh:/entrypoint.sh:ro" \
  "${image}" bash /entrypoint.sh >/dev/null

server_ready=0
for _ in $(seq 1 600); do
  if ssh-keyscan -T 1 -p "${port}" 127.0.0.1 >/dev/null 2>&1; then
    server_ready=1
    break
  fi
  if [[ "$(docker inspect --format '{{.State.Running}}' "${container_name}" 2>/dev/null)" != "true" ]]; then
    break
  fi
  sleep 0.1
done
if [[ "${server_ready}" -ne 1 ]]; then
  docker logs "${container_name}"
  exit 1
fi

eval "$(ssh-agent -s)"
agent_pid="${SSH_AGENT_PID}"
ssh-add "${interop_dir}/certificate_key"

export CSHELL_OPENSSH_INTEROP=1
export CSHELL_OPENSSH_ADDRESS="127.0.0.1:${port}"
export CSHELL_OPENSSH_USERNAME=cshellinterop
export CSHELL_OPENSSH_HOST_FINGERPRINT
CSHELL_OPENSSH_HOST_FINGERPRINT="$(ssh-keygen -q -l -E sha256 -f "${interop_dir}/host_key.pub" | awk '{print $2}')"
export CSHELL_OPENSSH_PUBLIC_KEY="${interop_dir}/public_key"
export CSHELL_OPENSSH_CERTIFICATE_KEY="${interop_dir}/certificate_key"
export CSHELL_OPENSSH_CERTIFICATE="${interop_dir}/certificate_key-cert.pub"

docker logs "${container_name}"
"${cargo_command}" test -p cshell-ssh --test openssh_interop --all-features --locked -- --test-threads=1
