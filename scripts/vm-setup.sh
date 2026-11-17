#!/usr/bin/env bash
# The Linux test VM's setup, run as root INSIDE the guest by
# fs-linux-test-harness once per boot. Installs the oracles the FAT
# validation (validation/generate.sh) uses and records their versions.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq dosfstools mtools openssl python3
modprobe vfat || true
modprobe loop || true
grep -qw vfat /proc/filesystems || { echo "vm-setup: kernel has no vfat" >&2; exit 1; }
echo "vm-setup: $(uname -r) vfat; $(mkfs.fat --help 2>&1 | head -1); $(mdir --version 2>&1 | head -1)"
