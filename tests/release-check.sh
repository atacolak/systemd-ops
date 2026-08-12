#!/usr/bin/env bash
# Heavy pre-release check. Runs the fast gates, then the live suites on
# this host, then boots disposable QEMU guests and runs them again
# inside each one.
#
# What the guests test that nothing else can:
#   - the initrd boot phase, which is zero on every other environment
#     (CI runners never finish booting, containers never boot at all,
#     and a GRUB host leaves the value unset)
#   - enablement surviving a reboot, which is what enable means
#   - the systemd version boundary where the varlink socket appears
#
# Firmware and loader timestamps stay zero here too: those come from
# EFI variables that systemd-boot sets, and stock cloud images use
# GRUB. Covering them needs a UKI or sd-boot image (mkosi).
#
# Usage:
#   tests/release-check.sh                 # check only
#   tests/release-check.sh --tag v0.4.1    # check, then tag on success
#   IMAGES="name=url ..." tests/release-check.sh
set -euo pipefail

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
CACHE=${XDG_CACHE_HOME:-$HOME/.cache}/systemd-mcpd-vm
TAG=
[ "${1:-}" = "--tag" ] && TAG=${2:?--tag needs a version}

# Guests to boot, as name=url. Each distro release pins a systemd
# version; pick releases that straddle 258, where PID 1 starts serving
# the varlink socket.
IMAGES=${IMAGES:-"\
debian-13=https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.qcow2 \
fedora-43=https://download.fedoraproject.org/pub/fedora/linux/releases/43/Cloud/x86_64/images/Fedora-Cloud-Base-Generic-43-1.4.x86_64.qcow2"}

say()  { printf '\n== %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

preflight() {
  local missing=()
  for c in qemu-system-x86_64 qemu-img genisoimage ssh ssh-keygen curl jq; do
    command -v "$c" >/dev/null || missing+=("$c")
  done
  OVMF=$(ls /usr/share/OVMF/OVMF_CODE.fd /usr/share/ovmf/OVMF.fd \
             /usr/share/OVMF/OVMF_CODE_4M.fd 2>/dev/null | head -1 || true)
  [ -n "$OVMF" ] || missing+=("ovmf")
  [ -e /dev/kvm ] || fail "/dev/kvm is absent; this check needs hardware virtualization"
  if [ ${#missing[@]} -gt 0 ]; then
    fail "missing: ${missing[*]}
  sudo apt install qemu-system-x86 qemu-utils ovmf genisoimage curl jq"
  fi
}

gates() {
  say "fast gates"
  cd "$REPO"
  cargo fmt --check || fail "cargo fmt"
  cargo clippy --all-targets -- -D warnings || fail "clippy"
  cargo test || fail "cargo test"
  cargo build --release || fail "release build"
  rustup target list --installed | grep -q x86_64-unknown-linux-musl ||
    rustup target add x86_64-unknown-linux-musl
  cargo build --release --target x86_64-unknown-linux-musl || fail "musl build"
}

host_suites() {
  say "live suites on this host"
  MCPD=$REPO/target/release/systemd-mcpd HOST= \
    sudo /usr/bin/bash "$REPO/tests/integration.sh" || fail "integration.sh (host)"
  MCPD=$REPO/target/release/systemd-mcpd HOST= \
    sudo /usr/bin/bash "$REPO/tests/varlink-proof.sh" || fail "varlink-proof.sh (host)"
}

# Boots one guest, runs both suites inside it, then the assertions that
# only a real boot can make. Everything lands in a temp dir that the
# trap removes, and the guest disk is a throwaway overlay on the cached
# image, so the download is reused and never written to.
vm_suite() {
  local name=$1 url=$2
  local work port pid ssh_opts SSH HOST_PREFIX
  work=$(mktemp -d) port=$((2200 + RANDOM % 300))
  # shellcheck disable=SC2064
  trap "vm_teardown '$work'" RETURN

  mkdir -p "$CACHE"
  local base="$CACHE/$name.qcow2"
  [ -s "$base" ] || {
    say "$name: downloading image"
    curl -fL --retry 3 -o "$base.part" "$url" || fail "$name: download"
    mv "$base.part" "$base"
  }

  say "$name: booting"
  qemu-img create -q -f qcow2 -F qcow2 -b "$base" "$work/disk.qcow2" 20G
  ssh-keygen -q -t ed25519 -N '' -f "$work/key"
  # cloud-init gives the default user passwordless sudo, which is what
  # the suites need; HOST carries the sudo prefix.
  printf '#cloud-config\nssh_authorized_keys:\n  - %s\n' "$(cat "$work/key.pub")" >"$work/user-data"
  printf 'instance-id: mcpd\nlocal-hostname: mcpd\n' >"$work/meta-data"
  genisoimage -quiet -output "$work/seed.iso" -volid cidata -joliet -rock \
    "$work/user-data" "$work/meta-data"

  qemu-system-x86_64 -enable-kvm -m 2048 -smp 2 -nographic -no-reboot \
    -bios "$OVMF" \
    -drive file="$work/disk.qcow2",if=virtio \
    -drive file="$work/seed.iso",if=virtio,format=raw,readonly=on \
    -netdev user,id=n0,hostfwd=tcp:127.0.0.1:"$port"-:22 \
    -device virtio-net-pci,netdev=n0 \
    >"$work/console.log" 2>&1 &
  pid=$!
  echo "$pid" >"$work/qemu.pid"

  ssh_opts="-q -i $work/key -p $port -o BatchMode=yes -o StrictHostKeyChecking=no \
            -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5"
  # The default user differs per distro; try each until one answers.
  local user
  for _ in $(seq 60); do
    for u in debian fedora cloud-user ubuntu; do
      # shellcheck disable=SC2086
      if ssh $ssh_opts "$u@127.0.0.1" true 2>/dev/null; then user=$u; break 2; fi
    done
    kill -0 "$pid" 2>/dev/null || fail "$name: qemu exited; see $work/console.log"
    sleep 2
  done
  [ -n "${user:-}" ] || fail "$name: no SSH after 120s; see $work/console.log"
  # shellcheck disable=SC2086
  SSH="ssh $ssh_opts $user@127.0.0.1"
  $SSH sudo systemctl is-system-running --wait >/dev/null 2>&1 || true
  say "$name: guest is $($SSH systemctl --version | head -1)"

  # shellcheck disable=SC2086
  scp $ssh_opts "$REPO/target/x86_64-unknown-linux-musl/release/systemd-mcpd" \
    "$user@127.0.0.1:/tmp/systemd-mcpd" >/dev/null || fail "$name: scp"
  $SSH sudo install -m755 /tmp/systemd-mcpd /usr/local/bin/systemd-mcpd

  say "$name: live suites in guest"
  MCPD="$SSH sudo /usr/local/bin/systemd-mcpd" HOST="$SSH sudo" \
    bash "$REPO/tests/integration.sh" || fail "$name: integration.sh"
  MCPD="$SSH sudo /usr/local/bin/systemd-mcpd" HOST="$SSH sudo" \
    MCPD_NO_PATH="$SSH sudo env PATH=/nonexistent /usr/local/bin/systemd-mcpd" \
    bash "$REPO/tests/varlink-proof.sh" || vm_note "$name: varlink-proof skipped or failed (needs systemd >= 258)"

  vm_boot_phases "$name" "$SSH"
  vm_reboot_persistence "$name" "$SSH" "$ssh_opts" "$user" "$pid"
}

vm_note() { printf 'NOTE: %s\n' "$*"; }

vm_teardown() {
  local work=$1
  [ -f "$work/qemu.pid" ] && kill "$(cat "$work/qemu.pid")" 2>/dev/null || true
  rm -rf "$work"
}

# A guest boots with an initrd, so the phase is non-zero here and the
# totals must match what systemd-analyze reports.
vm_boot_phases() {
  local name=$1 SSH=$2 times analyze
  say "$name: boot phases against systemd-analyze"
  times=$(printf '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"boot_times","arguments":{}}}\n' |
    $SSH sudo /usr/local/bin/systemd-mcpd --grant boot:read |
    jq -r '.result.content[0].text')
  jq -e '.initrd_usec > 0' <<<"$times" >/dev/null ||
    fail "$name: initrd phase is zero in a guest that booted with an initrd: $times"
  jq -e '.kernel_usec > 0 and .userspace_usec > 0 and .total_usec > 0' <<<"$times" >/dev/null ||
    fail "$name: implausible boot_times: $times"
  # systemd-analyze prints microseconds with --json=short in >= 256; on
  # older guests fall back to comparing against the manager property.
  analyze=$($SSH sudo systemctl show --property=FinishTimestampMonotonic --value)
  [ "$(jq -r '.total_usec' <<<"$times")" -ge "$analyze" ] ||
    fail "$name: total_usec below FinishTimestampMonotonic ($times vs $analyze)"
  printf '   initrd=%s kernel=%s userspace=%s total=%s\n' \
    "$(jq -r .initrd_usec <<<"$times")" "$(jq -r .kernel_usec <<<"$times")" \
    "$(jq -r .userspace_usec <<<"$times")" "$(jq -r .total_usec <<<"$times")"
}

# enable means "starts at next boot". Nothing short of a reboot checks
# that, so this is the assertion the guest exists for.
vm_reboot_persistence() {
  local name=$1 SSH=$2 ssh_opts=$3 user=$4 pid=$5 unit=mcpd-persist.service
  say "$name: enablement survives a reboot"
  $SSH sudo bash -c "printf '[Unit]\nDescription=mcpd reboot persistence\n[Service]\nType=oneshot\nRemainAfterExit=yes\nExecStart=/bin/true\n[Install]\nWantedBy=multi-user.target\n' > /etc/systemd/system/$unit && systemctl daemon-reload"
  # Enable it through the server's own write path, not systemctl.
  local plan
  plan=$(printf '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"plan_change","arguments":{"action":"enable","unit":"%s"}}}\n{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"apply_plan","arguments":{"plan":1}}}\n' "$unit" |
    $SSH sudo /usr/local/bin/systemd-mcpd --grant units:write | tail -1)
  jq -e '.result.content[0].text | fromjson | .diff.unit_file_state.after == "enabled"' <<<"$plan" >/dev/null ||
    fail "$name: apply did not enable the unit: $plan"

  $SSH sudo systemctl reboot >/dev/null 2>&1 || true
  sleep 5
  local back=
  for _ in $(seq 60); do
    # shellcheck disable=SC2086
    if ssh $ssh_opts "$user@127.0.0.1" true 2>/dev/null; then back=1; break; fi
    kill -0 "$pid" 2>/dev/null || fail "$name: guest did not come back (qemu exited)"
    sleep 2
  done
  [ -n "$back" ] || fail "$name: guest did not come back after reboot"
  [ "$($SSH sudo systemctl is-active $unit)" = active ] ||
    fail "$name: $unit did not start on the next boot; enable did not persist"
  printf '   %s active after reboot\n' "$unit"
}

preflight
gates
host_suites
for entry in $IMAGES; do
  vm_suite "${entry%%=*}" "${entry#*=}"
done

say "PASS: release check complete"
if [ -n "$TAG" ]; then
  git -C "$REPO" tag -a "$TAG" -m "$TAG" && echo "tagged $TAG (push with: git push origin $TAG)"
fi
