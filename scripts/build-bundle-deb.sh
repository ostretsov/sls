#!/bin/bash
# Builds a self-contained package: target/sls_<version>_amd64.deb
#
# It carries its own GTK 4 and every library it needs in /opt/sls.  From the
# system it needs only libraries every Ubuntu desktop has with a stable ABI
# (glibc, OpenGL/Mesa, X11, fonts, ...; see EXCLUDE).  Build it on the oldest
# Ubuntu you want to support (glibc is forward compatible only).
#
#   sudo apt install ./target/sls_*_amd64.deb
set -euo pipefail
cd "$(dirname "$0")/.."

ARCH_LIB=/usr/lib/x86_64-linux-gnu
PREFIX=/opt/sls
PIXBUF_LOADERS="libpixbufloader-svg.so"

# Libraries taken from the system: same name and ABI on every Ubuntu desktop,
# or tied to the user's hardware/session (graphics drivers, X server).
EXCLUDE='^(ld-linux|libc|libm|libdl|libpthread|librt|libresolv|libutil|libgcc_s|libstdc\+\+|'\
'libGL|libGLX|libGLdispatch|libEGL|libGLESv2|libOpenGL|libgbm|libdrm|libvulkan|'\
'libX11|libX11-xcb|libxcb|libxcb-.*|libXau|libXdmcp|libXext|libXrender|libXi|libXfixes|libXdamage|'\
'libXrandr|libXcursor|libXinerama|libXcomposite|libxkbcommon|libwayland-.*|'\
'libfontconfig|libfreetype|libexpat|libz|libudev|libsystemd|libdbus-1|libselinux|libmount|libblkid|'\
'libuuid|libgcrypt|libgpg-error|liblzma|libzstd|libbz2|libcap|liblz4)\.so'

version=$(sed -n 's/^version = "\(.*\)"/\1/p' crates/app/Cargo.toml | head -1)
cargo build --release

root=target/bundle/sls_${version}_amd64
opt=$root$PREFIX
rm -rf "$root"
mkdir -p "$opt/bin" "$opt/libexec" "$opt/lib/gdk-pixbuf-2.0/loaders" "$opt/lib/gio/modules" \
  "$opt/share/glib-2.0/schemas"

install -m755 target/release/sls "$opt/libexec/sls"
binaries=("$opt/libexec/sls")
for l in $PIXBUF_LOADERS; do
  install -m644 "$ARCH_LIB/gdk-pixbuf-2.0/2.10.0/loaders/$l" "$opt/lib/gdk-pixbuf-2.0/loaders/"
  binaries+=("$opt/lib/gdk-pixbuf-2.0/loaders/$l")
done

# Every library these need (recursively; ldd resolves the whole tree).
missing=$(ldd "${binaries[@]}" 2>/dev/null | grep 'not found' || true)
if [ -n "$missing" ]; then echo "unresolved libraries:"; echo "$missing"; exit 1; fi
: > "$root.syslibs"
ldd "${binaries[@]}" 2>/dev/null | awk '/=> \//{print $3}' | sort -u | while read -r lib; do
  name=$(basename "$lib")
  if [[ $name =~ $EXCLUDE ]]; then
    echo "$lib" >> "$root.syslibs"
  else
    cp -L "$lib" "$opt/lib/$name"
  fi
done

# The pixbuf loader cache must name the final install path.
GDK_PIXBUF_MODULEDIR="$opt/lib/gdk-pixbuf-2.0/loaders" \
  "$ARCH_LIB/gdk-pixbuf-2.0/gdk-pixbuf-query-loaders" "$opt"/lib/gdk-pixbuf-2.0/loaders/*.so \
  | sed "s|$(realpath "$opt")|$PREFIX|g" > "$opt/lib/gdk-pixbuf-2.0/loaders.cache"
if grep '^"/' "$opt/lib/gdk-pixbuf-2.0/loaders.cache" | grep -v "^\"$PREFIX/"; then
  echo "loaders.cache names paths outside $PREFIX (above)"
  exit 1
fi

# GTK's own settings schemas (the folder chooser aborts without them; a
# desktop without GTK 4 installed does not have them).
cp /usr/share/glib-2.0/schemas/org.gtk.gtk4.Settings.*.gschema.xml "$opt/share/glib-2.0/schemas/"
glib-compile-schemas "$opt/share/glib-2.0/schemas"

cat > "$opt/bin/sls" <<'LAUNCHER'
#!/bin/sh
# Runs sls with the libraries bundled in /opt/sls.
HERE=/opt/sls
# Programs sls starts (xdg-open) get the original environment back.
export SLS_BUNDLE=1
export SLS_ORIG_LD_LIBRARY_PATH="${LD_LIBRARY_PATH-}"
export LD_LIBRARY_PATH="$HERE/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
# Bundled GLib/GTK must not load the system's (newer) modules.
export GIO_MODULE_DIR="$HERE/lib/gio/modules"
export GDK_PIXBUF_MODULE_FILE="$HERE/lib/gdk-pixbuf-2.0/loaders.cache"
export GSETTINGS_SCHEMA_DIR="$HERE/share/glib-2.0/schemas"
exec "$HERE/libexec/sls" "$@"
LAUNCHER
chmod 755 "$opt/bin/sls"

mkdir -p "$root/usr/bin"
ln -s "$PREFIX/bin/sls" "$root/usr/bin/sls"
# The menu entry and icon are named after the GTK application id, so GNOME
# matches the running window to them (dock icon, "Add to Favorites").
app_id=io.github.ostretsov.sls
install -Dm644 packaging/$app_id.desktop "$root/usr/share/applications/$app_id.desktop"
# The -attention variant is the tray icon while a file offer waits.
for icon in $app_id $app_id-attention; do
  install -Dm644 packaging/$icon.svg "$root/usr/share/icons/hicolor/scalable/apps/$icon.svg"
  for size in 24 32 48 64 128 256; do
    dir="$root/usr/share/icons/hicolor/${size}x$size/apps"
    mkdir -p "$dir"
    rsvg-convert -w $size -h $size packaging/$icon.svg -o "$dir/$icon.png"
  done
done
# Not the builder's umask (group-writable on Ubuntu).
find "$root/usr" -type d -exec chmod 755 {} +
find "$root/usr" -type f -exec chmod 644 {} +
install -Dm644 README.md "$root/usr/share/doc/sls/README.md"

# Runtime dependencies: the system packages that own the libraries we did not
# bundle (found with dpkg, so nothing is forgotten).
# libGL/libEGL are loaded at runtime (dlopen by libepoxy), invisible to ldd;
# the icons and xdg-open are used by name.
depends="libc6 (>= $(dpkg-query -W -f='${Version}' libc6 | cut -d- -f1)), libgl1, libegl1, adwaita-icon-theme, xdg-utils"
owner() { # packages register either /lib or /usr/lib paths (merged /usr)
  local real; real=$(realpath "$1")
  for path in "$1" "$real" "/usr$1" "${real#/usr}"; do
    dpkg -S "$path" 2>/dev/null < /dev/null && return
  done
  echo "no package owns $1" >&2
  exit 1
}
for pkg in $(while read -r lib; do owner "$lib"; done < "$root.syslibs" | cut -d: -f1 | sort -u); do
  [ "$pkg" = libc6 ] || depends="$depends, $pkg"
done
rm -f "$root.syslibs"
echo "Depends: $depends"

size=$(du -sk "$root" | cut -f1)
mkdir -p "$root/DEBIAN"
cat > "$root/DEBIAN/control" <<CONTROL
Package: sls
Version: $version
Architecture: amd64
Maintainer: Artem Ostretsov <ostretsovaa@gmail.com>
Section: net
Priority: optional
Installed-Size: $size
Depends: $depends
Description: Simple LAN share: share folders with the other computers on your network
 Pick folders to share; every computer on the local network running sls finds
 the others by itself and shows what they share.  Browse and download files
 and whole folders; phones and other computers can use a web browser.
 .
 Self-contained: GTK 4 and its libraries are bundled in /opt/sls.
CONTROL

# A running instance keeps the old binary; tell the user.
cat > "$root/DEBIAN/postinst" <<'POSTINST'
#!/bin/sh
set -e
if [ "$1" = configure ] && pgrep -x sls >/dev/null 2>&1; then
  echo "sls is running: quit it (top bar icon, Quit) and start it again to use the new version."
fi
exit 0
POSTINST
chmod 755 "$root/DEBIAN/postinst"

dpkg-deb --root-owner-group -Zxz --build "$root" "target/sls_${version}_amd64.deb"
echo "Built target/sls_${version}_amd64.deb ($(du -h "target/sls_${version}_amd64.deb" | cut -f1), $(ls "$opt/lib" | wc -l) bundled libraries)"
