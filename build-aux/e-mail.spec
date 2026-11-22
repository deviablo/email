# SPDX-License-Identifier: GPL-3.0-or-later
#
# Packs the files `meson install` put into %%{stage}. Built by
# build-aux/package-rpm.sh; dependencies on shared libraries are found by
# rpm itself, so the package works on Fedora and openSUSE Tumbleweed.
%global debug_package %{nil}
%global _build_id_links none

Name:           e-mail
Version:        %{version}
Release:        1
Summary:        Simple, modern email
License:        GPL-3.0-or-later
URL:            https://github.com/deviablo/email
Requires:       hicolor-icon-theme
Recommends:     xdg-desktop-portal

%description
E-Mail is a simple email app for GNOME and KDE. It connects to your
accounts with IMAP and SMTP and does nothing else: no calendar, no chat,
no contact management.

%install
cp -a %{stage}/. %{buildroot}/
install -Dm644 %{license_file} %{buildroot}%{_datadir}/licenses/e-mail/LICENSE

%files
%license %{_datadir}/licenses/e-mail/LICENSE
%{_bindir}/e-mail
%{_datadir}/applications/io.github.deviablo.EMail.desktop
%{_datadir}/metainfo/io.github.deviablo.EMail.metainfo.xml
%{_datadir}/glib-2.0/schemas/io.github.deviablo.EMail.gschema.xml
%{_datadir}/dbus-1/services/io.github.deviablo.EMail.service
%{_datadir}/icons/hicolor/scalable/apps/io.github.deviablo.EMail.svg
%{_datadir}/icons/hicolor/symbolic/apps/io.github.deviablo.EMail-symbolic.svg
%{_datadir}/icons/hicolor/symbolic/apps/io.github.deviablo.EMail-*-symbolic.svg
%{_datadir}/locale/*/LC_MESSAGES/e-mail.mo
