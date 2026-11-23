#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Builds the .deb package (Ubuntu, Debian, Linux Mint) from the source tree.
# Dependencies on shared libraries are found by dpkg-shlibdeps, so the
# package asks for exactly the libraries the program uses.
# Usage: build-aux/package-deb.sh OUTDIR
set -eu
out="$(realpath -m "$1")"
mkdir -p "$out"
version="$(sed -n "s/^  version: '\(.*\)',$/\1/p" meson.build)"
root="$(mktemp -d)"
work="$(mktemp -d)"

meson setup build-deb --prefix=/usr --buildtype=release
meson compile -C build-deb
DESTDIR="$root" meson install -C build-deb --no-rebuild

mkdir -p "$work/debian"
printf 'Source: e-mail\n\nPackage: e-mail\nArchitecture: any\n' > "$work/debian/control"
depends="$(cd "$work" && dpkg-shlibdeps -O -e"$root/usr/bin/e-mail" 2>/dev/null \
  | sed -n 's/^shlibs:Depends=//p')"
arch="$(dpkg --print-architecture)"
size="$(du -sk "$root" | cut -f1)"

# Lets WebKit start its sandbox on Ubuntu, see data/apparmor/.
profile=io.github.deviablo.EMail
install -Dm644 "data/apparmor/$profile" "$root/etc/apparmor.d/$profile"

mkdir -p "$root/DEBIAN"
echo "/etc/apparmor.d/$profile" > "$root/DEBIAN/conffiles"
cat > "$root/DEBIAN/postinst" <<SCRIPT
#!/bin/sh
set -e
if [ "\$1" = configure ] && command -v apparmor_parser >/dev/null 2>&1 \\
    && [ -d /sys/kernel/security/apparmor ]; then
  apparmor_parser -r -T -W /etc/apparmor.d/$profile || true
fi
SCRIPT
cat > "$root/DEBIAN/prerm" <<SCRIPT
#!/bin/sh
set -e
if [ "\$1" = remove ] && command -v apparmor_parser >/dev/null 2>&1 \\
    && [ -d /sys/kernel/security/apparmor ]; then
  apparmor_parser -R /etc/apparmor.d/$profile 2>/dev/null || true
fi
SCRIPT
chmod 755 "$root/DEBIAN/postinst" "$root/DEBIAN/prerm"
cat > "$root/DEBIAN/control" <<CONTROL
Package: e-mail
Version: $version
Architecture: $arch
Maintainer: The E-Mail developers <317311006+deviablo@users.noreply.github.com>
Installed-Size: $size
Depends: $depends
Recommends: xdg-desktop-portal, librsvg2-common, gnome-keyring | kwalletmanager | keepassxc
Section: mail
Priority: optional
Homepage: https://github.com/deviablo/email
Description: Simple, modern email
 E-Mail is a simple email app for GNOME and KDE. It connects to your
 accounts with IMAP and SMTP and does nothing else: no calendar, no chat,
 no contact management.
CONTROL

dpkg-deb --root-owner-group --build "$root" "$out/e-mail_${version}_${arch}.deb"
rm -rf "$root" "$work"
