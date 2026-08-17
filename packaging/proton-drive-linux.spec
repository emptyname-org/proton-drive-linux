# Local in-tree RPM (mirrors packaging/PKGBUILD).
# From the repository root:
#   rpmbuild -bb packaging/proton-drive-linux.spec --define "git_dir $PWD" ...
# %build needs network so cargo can fetch crates.io (same trade-off as the Arch PKGBUILD).

%global debug_package %{nil}
# Match Arch PKGBUILD `!lto` — LTO has broken some GTK/Rust links in practice.
%global _lto_cflags %{nil}

%{!?version: %global version 1.8.2+fork.2}

Name:           proton-drive-linux
Version:        %{version}
Release:        1%{?dist}
Summary:        Proton Drive client for Linux (FUSE, CLI, GTK4 app + tray)
License:        MIT
URL:            https://github.com/emptyname-org/proton-drive-linux
ExclusiveArch:  x86_64

BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  pkgconf-pkg-config
BuildRequires:  fuse3-devel
BuildRequires:  gtk4-devel
BuildRequires:  libadwaita-devel
BuildRequires:  libsecret-devel
BuildRequires:  dbus-devel
BuildRequires:  glib2-devel
BuildRequires:  webkitgtk6.0-devel

Requires:       fuse3
Requires:       gtk4
Requires:       libadwaita
Requires:       libsecret
Requires:       webkitgtk6.0
Requires:       xdg-utils
Requires:       perl-Image-ExifTool

# DE-specific; do not Require a single desktop environment.
Recommends:     gnome-keyring
Recommends:     gnome-shell-extension-appindicator
Recommends:     kwallet

Provides:       pdfs = %{version}-%{release}

%description
Unofficial Proton Drive client featuring a FUSE files-on-demand mount,
CLI, GTK4/Libadwaita GUI, system tray, and search launcher.

%prep
# In-tree build: no Source tarball. Pass --define "git_dir /path/to/checkout".
test -n "%{?git_dir}" || (echo 'Pass --define "git_dir $PWD" from the repo root' >&2; exit 1)
test -f %{git_dir}/Cargo.toml
cp -a %{git_dir}/LICENSE %{git_dir}/packaging/ICON-LICENSE.md .
%build
cd %{git_dir}
target_dir="%{?cargo_target_dir}"
test -n "$target_dir" || target_dir="%{git_dir}/target"
CARGO_TARGET_DIR="$target_dir" cargo build --release --locked \
  --bin pdfs \
  --bin pdfs-tray \
  --bin pdfs-app \
  --bin pdfs-prompt

%install
target_dir="%{?cargo_target_dir}"
test -n "$target_dir" || target_dir="%{git_dir}/target"
rel="$target_dir/release"
install -D -m0755 "$rel/pdfs"        %{buildroot}%{_bindir}/pdfs
install -D -m0755 "$rel/pdfs-tray"   %{buildroot}%{_bindir}/pdfs-tray
install -D -m0755 "$rel/pdfs-app"    %{buildroot}%{_bindir}/pdfs-app
install -D -m0755 "$rel/pdfs-prompt" %{buildroot}%{_bindir}/pdfs-prompt

install -D -m0644 %{git_dir}/packaging/io.narl.proton-drive-linux.desktop \
  %{buildroot}%{_datadir}/applications/io.narl.proton-drive-linux.desktop
install -D -m0644 %{git_dir}/packaging/io.narl.proton-drive-linux-tray.desktop \
  %{buildroot}%{_sysconfdir}/xdg/autostart/io.narl.proton-drive-linux-tray.desktop
install -D -m0644 %{git_dir}/packaging/linux_cloud_folder_1.svg \
  %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/io.narl.proton-drive-linux.svg
install -D -m0644 %{git_dir}/packaging/proton-drive.service \
  %{buildroot}/usr/lib/systemd/user/proton-drive.service

%files
%license LICENSE ICON-LICENSE.md
%{_bindir}/pdfs
%{_bindir}/pdfs-tray
%{_bindir}/pdfs-app
%{_bindir}/pdfs-prompt
%{_datadir}/applications/io.narl.proton-drive-linux.desktop
%{_sysconfdir}/xdg/autostart/io.narl.proton-drive-linux-tray.desktop
%{_datadir}/icons/hicolor/scalable/apps/io.narl.proton-drive-linux.svg
/usr/lib/systemd/user/proton-drive.service

%changelog
* Mon Aug 17 2026 Proton Drive Linux contributors - 1.8.2+fork.2-1
- Standardize application dialogs on one native GTK window template.

* Mon Aug 17 2026 Proton Drive Linux contributors - 1.8.2+fork.1-1
- First experimental fork release with Files thumbnails and desktop UI changes.

* Mon Aug 17 2026 Proton Drive Linux contributors - 1.8.2-1
- Add local and camera RAW thumbnails, including ExifTool runtime support.
