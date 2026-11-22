# E-Mail entwickeln

Wie man E-Mail aus dem Quelltext baut, testet und die Pakete selbst erzeugt. Für Nutzer steht die Installation in der [README](../README.md).

## Inhalt

| Pfad | Inhalt |
|---|---|
| [`design/icon/`](../design/icon/) | App-Icon (farbig und einfarbig) |
| [`crates/core/`](../crates/core/) | Mail-Logik ohne Oberfläche: IMAP, lokaler Speicher, HTML-Säuberung |
| [`crates/app/`](../crates/app/) | Die App mit GTK 4 und libadwaita |
| [`data/`](../data/), [`po/`](../po/) | Desktop-Integration und Übersetzungen |

## Selbst bauen und ausprobieren

Zuerst die nötigen Pakete installieren.

Ubuntu:

```bash
sudo apt install cargo rustc meson ninja-build gettext libgtk-4-dev libadwaita-1-dev libwebkitgtk-6.0-dev desktop-file-utils appstream libglib2.0-dev-bin
```

Fedora:

```bash
sudo dnf install cargo rust meson gettext gtk4-devel libadwaita-devel webkitgtk6.0-devel desktop-file-utils appstream glib2-devel
```

Rust muss mindestens Version 1.89 haben (`rustc --version`). Ist die Version des Systems älter, wie bei Ubuntu 24.04, Rust stattdessen über [rustup](https://rustup.rs) installieren.

Dann bauen und für den eigenen Benutzer installieren (landet in `~/.local`, kein Administrator nötig):

```bash
meson setup builddir --prefix="$HOME/.local"
meson install -C builddir
```

Danach erscheint „E-Mail“ in der App-Übersicht. Wieder entfernen:

```bash
ninja -C builddir uninstall
```

Als Flatpak bauen und installieren:

```bash
flatpak remote-add --user --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo
flatpak-builder --user --install-deps-from=flathub --install --force-clean flatpak-build build-aux/flatpak/io.github.deviablo.EMail.json
flatpak run io.github.deviablo.EMail
```

## Tests

```bash
cargo test
```

Der Integrationstest gegen einen lokalen Test-Mailserver (GreenMail im Container) braucht Docker oder Podman und läuft in GitHub Actions automatisch:

```bash
build-aux/test-mail-server.sh /tmp/e-mail-test-server
E_MAIL_TEST_CA=/tmp/e-mail-test-server/ca.der cargo test -p e-mail-core --test mail_server -- --ignored
```

## Pakete selbst bauen

```bash
build-aux/package-deb.sh dist   # unter Ubuntu/Debian, braucht dpkg-dev
build-aux/package-rpm.sh dist   # unter Fedora, braucht rpm-build
```

Die Pakete baut in GitHub Actions der Workflow „Packages“ (`.github/workflows/packages.yml`): bei jedem Pull Request `.deb` und `.rpm` als herunterladbare Artefakte (das Flatpak-Bundle liefert der Workflow „CI“), bei einem Tag wie `v1.0.0` zusätzlich das Flatpak und ein GitHub Release mit allen drei Dateien.
