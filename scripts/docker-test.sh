#!/bin/bash
# Installs target/sls_*.deb in fresh Ubuntu containers and runs
# `sls --self-test` on a virtual display.  Usage: scripts/docker-test.sh [image...]
set -euo pipefail
cd "$(dirname "$0")/.."
images=("${@:-ubuntu:22.04 ubuntu:24.04 ubuntu:rolling}")
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cp target/sls_*_amd64.deb "$work/sls.deb"
cat > "$work/test.sh" <<'INNER'
#!/bin/bash
set -e
. /etc/os-release; echo "=== $PRETTY_NAME"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq >/dev/null
apt-get install -y -qq /work/sls.deb xvfb xauth >/tmp/apt.log 2>&1 || { tail -20 /tmp/apt.log; exit 1; }
unresolved=$(LD_LIBRARY_PATH=/opt/sls/lib ldd /opt/sls/libexec/* /opt/sls/lib/gdk-pixbuf-2.0/loaders/*.so 2>/dev/null \
  | grep "not found" | sort -u || true)
[ -z "$unresolved" ] || { echo "unresolved libraries:"; echo "$unresolved"; exit 1; }
cd /tmp && HOME=/tmp xvfb-run -a sls --self-test 2>&1 | grep -vE "DRI3|^\s*$"
exit "${PIPESTATUS[0]}"
INNER
chmod +x "$work/test.sh"
status=0
for img in ${images[@]}; do
  docker run --rm -v "$work:/work:ro" "$img" /work/test.sh || status=1
done
exit $status
