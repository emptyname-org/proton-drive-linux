#!/usr/bin/env bash

# Build the binary tarball and Debian package from already-built release
# binaries. This is shared by local release checks and GitHub Actions so the
# package being tested is the package being published.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
version="${1:-$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$repo_root/Cargo.toml" | head -n1)}"
output_dir="${2:-$repo_root/dist}"
release_dir="$repo_root/target/release"

if [[ -z "$version" || ! "$version" =~ ^[0-9][0-9A-Za-z.+:~-]*$ ]]; then
  printf 'Invalid package version: %q\n' "$version" >&2
  exit 2
fi

for binary in pdfs pdfs-tray pdfs-app pdfs-prompt; do
  if [[ ! -x "$release_dir/$binary" ]]; then
    printf 'Missing release binary: %s\n' "$release_dir/$binary" >&2
    printf 'Run cargo build --release --locked first.\n' >&2
    exit 1
  fi
done

mkdir -p "$output_dir"
output_dir="$(cd "$output_dir" && pwd)"
staging_dir="$(mktemp -d "${TMPDIR:-/tmp}/proton-drive-linux-package.XXXXXX")"

cleanup() {
  case "$staging_dir" in
    "${TMPDIR:-/tmp}"/proton-drive-linux-package.*) rm -rf -- "$staging_dir" ;;
    *) printf 'Refusing to remove unexpected staging path: %s\n' "$staging_dir" >&2 ;;
  esac
}
trap cleanup EXIT

source_date_epoch="${SOURCE_DATE_EPOCH:-$(git -C "$repo_root" log -1 --format=%ct)}"
export SOURCE_DATE_EPOCH="$source_date_epoch"
binary_stage="$staging_dir/binaries"
deb_stage="$staging_dir/deb"
mkdir -p "$binary_stage"

for binary in pdfs pdfs-tray pdfs-app pdfs-prompt; do
  install -m0755 "$release_dir/$binary" "$binary_stage/$binary"
done

tarball="$output_dir/proton-drive-linux-${version}-x86_64.tar.gz"
tar --sort=name \
  --mtime="@$source_date_epoch" \
  --owner=0 --group=0 --numeric-owner \
  -czf "$tarball" -C "$binary_stage" .

install -d \
  "$deb_stage/DEBIAN" \
  "$deb_stage/usr/bin" \
  "$deb_stage/usr/share/applications" \
  "$deb_stage/usr/share/icons/hicolor/scalable/apps" \
  "$deb_stage/usr/share/licenses/proton-drive-linux" \
  "$deb_stage/usr/lib/systemd/user" \
  "$deb_stage/etc/xdg/autostart"

for binary in pdfs pdfs-tray pdfs-app pdfs-prompt; do
  install -m0755 "$release_dir/$binary" "$deb_stage/usr/bin/$binary"
done
install -m0644 "$repo_root/packaging/io.narl.proton-drive-linux.desktop" \
  "$deb_stage/usr/share/applications/io.narl.proton-drive-linux.desktop"
install -m0644 "$repo_root/packaging/io.narl.proton-drive-linux-tray.desktop" \
  "$deb_stage/etc/xdg/autostart/io.narl.proton-drive-linux-tray.desktop"
install -m0644 "$repo_root/packaging/io.narl.proton-drive-linux.svg" \
  "$deb_stage/usr/share/icons/hicolor/scalable/apps/io.narl.proton-drive-linux.svg"
install -m0644 "$repo_root/packaging/proton-drive.service" \
  "$deb_stage/usr/lib/systemd/user/proton-drive.service"
install -m0644 "$repo_root/LICENSE" \
  "$deb_stage/usr/share/licenses/proton-drive-linux/LICENSE"

installed_size="$(du -sk "$deb_stage" | cut -f1)"
cat > "$deb_stage/DEBIAN/control" <<EOF
Package: proton-drive-linux
Version: $version
Section: utils
Priority: optional
Architecture: amd64
Maintainer: Proton Drive Linux contributors <noreply@github.com>
Homepage: https://github.com/narl/proton-drive-linux
Depends: fuse3, libgtk-4-1 (>= 4.8.0), libadwaita-1-0 (>= 1.2.0), libsecret-1-0, libwebkitgtk-6.0-4, libimage-exiftool-perl
Installed-Size: $installed_size
Description: Proton Drive client for Linux
 Proton Drive client for Linux featuring:
  - FUSE files-on-demand mount
  - Command-line interface (CLI)
  - GTK4 system tray, desktop application, and search launcher
EOF

find "$deb_stage" -exec touch -h -d "@$source_date_epoch" {} +
deb="$output_dir/proton-drive-linux_${version}_amd64.deb"
dpkg-deb --root-owner-group --build "$deb_stage" "$deb"

(
  cd "$output_dir"
  sha256sum "$(basename "$tarball")" "$(basename "$deb")" > SHA256SUMS
)

printf 'Created %s\n' "$tarball"
printf 'Created %s\n' "$deb"
printf 'Created %s\n' "$output_dir/SHA256SUMS"
