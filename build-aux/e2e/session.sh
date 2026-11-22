#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Runs inside `dbus-run-session`: a virtual screen with a window manager,
# the accessibility bus, an unlocked keyring and stand-ins for the
# notification service and the panel, then the scripted user tests.
# Usage: dbus-run-session -- build-aux/e2e/session.sh OUTDIR [SCRIPT]
set -u
out="$1"
mkdir -p "$out"
Xvfb :99 -screen 0 1500x1250x24 >/dev/null 2>&1 &
export DISPLAY=:99
sleep 2
openbox >/dev/null 2>&1 &
export GDK_BACKEND=x11
export GTK_A11Y=atspi
export NO_AT_BRIDGE=0
/usr/libexec/at-spi-bus-launcher --launch-immediately >/dev/null 2>&1 &
eval "$(printf 'e2e-keyring' | gnome-keyring-daemon --unlock --components=secrets)"
export GNOME_KEYRING_CONTROL
python3 build-aux/e2e/fake_notifications.py "$out/notifications.json" &
python3 build-aux/e2e/fake_watcher.py "$out/tray.json" &
sleep 2
python3 "${2:-build-aux/e2e/run.py}" "$out"
