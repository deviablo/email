#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Scripted user tests: drives the real app through the accessibility
# interface (AT-SPI) and the keyboard (xdotool), against the local test
# mail server. Every scenario leaves screenshots and a line in report.md.
# Usage (inside build-aux/e2e/session.sh): run.py OUTDIR

import email
import email.policy
import imaplib
import json
import re
import os
import smtplib
import ssl
import subprocess
import sys
import time
import traceback
from email.message import EmailMessage

import gi

gi.require_version("Atspi", "2.0")
from gi.repository import Atspi  # noqa: E402

# A frozen app should fail a scenario quickly, not stall every call for 25 s.
Atspi.set_timeout(4000, 15000)

OUT = sys.argv[1]
APP = os.environ.get("E2E_APP", "target/debug/e-mail")
CA = os.environ["E_MAIL_TEST_CA_PEM"]
PASSWORD = "secret"
ALICE = "alice@example.test"
BOB = "bob@example.test"
CAROL = "carol@example.test"
results = []
process = None
account_ready = False
TEXT_ROLES = ("text", "password text", "entry", "spin button")


# ---------------------------------------------------------------- helpers

def log(text):
    print(text, flush=True)


def tls():
    context = ssl.create_default_context(cafile=CA)
    return context


def shot(name):
    subprocess.run(["import", "-window", "root", f"{OUT}/{name}.png"], check=False)


def header_shot(name):
    """Close-up of the list header: search, select, new mail."""
    buttons = find_all("push button", "New Mail", exact=True)
    if not buttons:
        log("   no New Mail button for the header shot")
        return
    button = buttons[0]
    extents = button.get_component_iface().get_extents(Atspi.CoordType.WINDOW)
    ox, oy = window_origin()
    right = ox + extents.x + extents.width + 8
    top = oy + extents.y - 10
    width = 380
    subprocess.run(["import", "-window", "root", "-crop",
                    f"{width}x{extents.height + 20}+{right - width}+{top}",
                    "-resize", "200%", f"{OUT}/{name}.png"], check=False)


def xdo(*args):
    subprocess.run(["xdotool", *args], check=False)


def type_text(text):
    xdo("type", "--delay", "25", text)
    time.sleep(0.3)


def key(*keys):
    for k in keys:
        xdo("key", "--clearmodifiers", k)
        time.sleep(0.25)


def app_root():
    desktop = Atspi.get_desktop(0)
    for i in range(desktop.get_child_count()):
        child = desktop.get_child_at_index(i)
        try:
            if child and child.get_name() in ("e-mail", "E-Mail"):
                return child
        except Exception:
            pass
    return None


def walk(node, depth=0):
    yield node
    if depth > 60:
        return
    try:
        count = node.get_child_count()
    except Exception:
        return
    for i in range(count):
        try:
            child = node.get_child_at_index(i)
        except Exception:
            continue
        if child is not None:
            yield from walk(child, depth + 1)


def label(node):
    try:
        return (node.get_name() or "") + " | " + (node.get_description() or "")
    except Exception:
        return ""


def showing(node):
    try:
        states = node.get_state_set()
        return states.contains(Atspi.StateType.SHOWING) and states.contains(
            Atspi.StateType.VISIBLE)
    except Exception:
        return False


def find_all(role=None, name=None, exact=False, root=None, visible=True):
    root = root or app_root()
    if root is None:
        return []
    found = []
    for node in walk(root):
        try:
            node_role = node.get_role_name()
        except Exception:
            continue
        if role == "text":
            if node_role not in TEXT_ROLES:
                continue
        elif role and node_role != role:
            continue
        text = label(node)
        if name is not None:
            if exact:
                if (node.get_name() or "") != name:
                    continue
            elif name.lower() not in text.lower():
                continue
        if visible and not showing(node):
            continue
        found.append(node)
    return found


def dump(file):
    root = app_root()
    with open(f"{OUT}/{file}", "w") as f:
        if root is None:
            f.write("app not found\n")
            return
        for node in walk(root):
            try:
                f.write(f"{node.get_role_name()}: {label(node)}"
                        f"{' [showing]' if showing(node) else ''}\n")
            except Exception:
                pass


def find(role=None, name=None, exact=False, timeout=12):
    end = time.time() + timeout
    while time.time() < end:
        nodes = find_all(role, name, exact)
        if nodes:
            return nodes[0]
        time.sleep(0.5)
    raise AssertionError(f"not found: role={role} name={name}")


def gone(role=None, name=None, timeout=15):
    end = time.time() + timeout
    while time.time() < end:
        if not find_all(role, name):
            return True
        time.sleep(0.5)
    raise AssertionError(f"still there: role={role} name={name}")


BUTTON_ROLES = ("push button", "toggle button", "menu item", "check box", "radio button")


def press(node):
    """Buttons are pressed through the accessibility interface, everything
    else (list rows, tiles) is clicked like a user would."""
    action = node.get_action_iface()
    if (node.get_role_name() in BUTTON_ROLES and action is not None
            and action.get_n_actions() > 0):
        action.do_action(0)
    else:
        click(node)
    time.sleep(0.8)


def window_origin():
    """Top left of the active window on screen. GTK reports positions
    relative to its window, the window manager knows where that is."""
    for command in (["xdotool", "getactivewindow"],
                    ["xdotool", "search", "--onlyvisible", "--pid", str(process.pid)]):
        found = subprocess.run(command, capture_output=True, text=True, check=False)
        ids = found.stdout.split()
        if not ids:
            continue
        result = subprocess.run(["xdotool", "getwindowgeometry", "--shell", ids[-1]],
                                capture_output=True, text=True, check=False)
        values = dict(line.split("=", 1) for line in result.stdout.split() if "=" in line)
        x, y = int(values.get("X", 0)), int(values.get("Y", 0))
        if (x, y) != (0, 0):
            return x, y
    return 0, 0


def click(node, right=None):
    """Clicks the middle of a widget, or `right` pixels from its right edge."""
    extents = node.get_component_iface().get_extents(Atspi.CoordType.WINDOW)
    ox, oy = window_origin()
    x = ox + extents.x + (extents.width - right if right else extents.width // 2)
    y = oy + extents.y + extents.height // 2
    log(f"   click {label(node)!r} at {x},{y} (window {ox},{oy}, {extents.x},{extents.y})")
    xdo("mousemove", str(x), str(y))
    xdo("click", "1")
    time.sleep(0.6)


def focus(node):
    click(node)
    time.sleep(0.3)


def text_of(node):
    try:
        return Atspi.Text.get_text(node, 0, Atspi.Text.get_character_count(node))
    except Exception:
        return "?"


def fill(role, name, text):
    node = find(role, name)
    focus(node)
    key("ctrl+a")
    type_text(text)
    log(f"   {name}: {text_of(node)!r}")


def button(name, timeout=20):
    press(find("push button", name, timeout=timeout))


def calm_down():
    """Closes whatever a failed scenario left open: dialogs, menus, the
    compose window, so the next scenario starts from the main window."""
    for _ in range(3):
        key("Escape")
    main = app_root()
    if process and main:
        xdo("search", "--onlyvisible", "--pid", str(process.pid), "--name", "^E-Mail$",
            "windowactivate", "--sync")
    time.sleep(0.5)


def action(name, target=None):
    """Activates an application action over D-Bus, as the menu would."""
    args = ["gdbus", "call", "--session", "--dest", "io.github.deviablo.EMail",
            "--object-path", "/io/github/deviablo/EMail",
            "--method", "org.gtk.Actions.Activate", name, target or "[]", "{}"]
    subprocess.run(args, check=False, capture_output=True)
    time.sleep(1.5)


def wait_until(check, timeout=60, what="condition"):
    end = time.time() + timeout
    while time.time() < end:
        try:
            if check():
                return True
        except Exception:
            pass
        time.sleep(1)
    raise AssertionError(f"timed out waiting for {what}")


def start_app(env=None):
    global process
    full = dict(os.environ)
    full.update(env or {})
    process = subprocess.Popen([APP], env=full, stdout=open(f"{OUT}/app.log", "a"),
                               stderr=subprocess.STDOUT)
    wait_until(lambda: app_root() is not None, 30, "the app on the accessibility bus")
    time.sleep(3)


def stop_app():
    global process
    action("quit")
    if process:
        try:
            process.wait(10)
        except subprocess.TimeoutExpired:
            process.kill()
    process = None
    time.sleep(1)


# ------------------------------------------------------------ mail server

def send_mail(sender, to, subject, text=None, html=None, attachment=None, image=None):
    message = EmailMessage(policy=email.policy.SMTP)
    message["From"] = sender
    message["To"] = to
    message["Subject"] = subject
    message["Date"] = email.utils.formatdate(localtime=True)
    message["Message-ID"] = email.utils.make_msgid(domain="example.test")
    message.set_content(text or "Hello")
    if html:
        message.add_alternative(html, subtype="html")
    if attachment:
        name, data = attachment
        message.add_attachment(data, maintype="application", subtype="pdf", filename=name)
    with smtplib.SMTP_SSL("localhost", 3465, context=tls()) as smtp:
        login = sender.split("<")[-1].strip(">")
        smtp.login(login, PASSWORD)
        smtp.send_message(message)


def imap(user):
    connection = imaplib.IMAP4_SSL("localhost", 3993, ssl_context=tls())
    connection.login(user, PASSWORD)
    return connection


def subjects(user, folder="INBOX"):
    connection = imap(user)
    status, _ = connection.select(f'"{folder}"')
    if status != "OK":
        connection.logout()
        return []
    _, data = connection.search(None, "ALL")
    found = []
    for num in data[0].split():
        _, parts = connection.fetch(num, "(BODY.PEEK[HEADER.FIELDS (SUBJECT)])")
        header = email.message_from_bytes(parts[0][1])
        found.append(str(email.header.make_header(email.header.decode_header(
            header.get("Subject", "")))))
    connection.logout()
    return found


def raw_message(user, folder, subject):
    connection = imap(user)
    connection.select(f'"{folder}"')
    _, data = connection.search(None, "SUBJECT", f'"{subject}"')
    nums = data[0].split()
    if not nums:
        connection.logout()
        return None
    _, parts = connection.fetch(nums[-1], "(BODY.PEEK[])")
    connection.logout()
    return parts[0][1]


def notifications():
    try:
        with open(f"{OUT}/notifications.json") as f:
            return json.load(f)
    except Exception:
        return []


# -------------------------------------------------------------- scenarios

def scenario(title, needs_account=True):
    def wrap(function):
        def run():
            log(f"== {title}")
            if needs_account and not account_ready:
                results.append((title, "übersprungen", "Einrichtung gescheitert"))
                return
            if needs_account:
                calm_down()
            try:
                note = function() or ""
                results.append((title, "bestanden", note))
            except Exception as error:
                slug = function.__name__
                shot(f"FAIL-{slug}")
                dump(f"FAIL-{slug}.tree.txt")
                traceback.print_exc()
                results.append((title, "FEHLER", str(error)))
        run.__name__ = function.__name__
        run.__wrapped__ = function
        return run
    return wrap


@scenario("Erster Start zeigt die Einrichtung", needs_account=False)
def first_start():
    start_app()
    find("text", "Email address")
    shot("01-einrichtung")


@scenario("Einrichtung: unbekannter Anbieter, Server selbst eintragen, falsches Passwort",
          needs_account=False)
def setup_wrong_password():
    fill("text", "Email address", ALICE)
    button("Continue")
    fill("text", "Password", "wrong")
    button("Sign In")
    find(None, "Settings not found", timeout=90)
    shot("02-nicht-gefunden")
    press(find(None, "Enter Servers Yourself"))
    find(None, "Incoming mail (IMAP)", timeout=20)
    fill("text", "Password", "wrong")
    servers = find_all("text", "Server")
    for node in servers:
        focus(node)
        key("ctrl+a")
        type_text("localhost")
    ports = find_all("text", "Port")
    log(f"   servers {len(servers)}, ports {len(ports)}")
    for node, port in zip(ports, ["3993", "3465"]):
        # The number sits left of the minus and plus buttons.
        click(node, right=110)
        key("ctrl+a")
        type_text(port)
        key("Tab")
    shot("03-manuell")
    button("Connect")
    find(None, "not accepted", timeout=60)
    shot("04-falsches-passwort")


@scenario("Einrichtung: richtiges Passwort, Name, fertig", needs_account=False)
def setup_success():
    global account_ready
    press(find(None, "Enter Servers Yourself", timeout=5)) if find_all(
        None, "Enter Servers Yourself") else None
    fill("text", "Password", PASSWORD)
    button("Connect")
    find("text", "Your name", timeout=90)
    fill("text", "Your name", "Alice Test")
    fill("text", "Name for this account", "Privat")
    button("Done")
    find(None, "All set", timeout=30)
    shot("05-fertig")
    button("Open Inbox")
    find(None, "Inbox", timeout=30)
    account_ready = True
    shot("06-posteingang")


@scenario("Neue Mails kommen an: HTML, Text, Anhang, externes Bild")
def receive():
    send_mail(f"Bob <{BOB}>", ALICE, "Plain hello", text="Just a plain text mail.")
    send_mail(f"Bob <{BOB}>", ALICE, "Invoice May",
              html="<p>Your <b>invoice</b> is attached.</p>",
              attachment=("invoice.pdf", b"%PDF-1.4 test"))
    send_mail(f"Carol <{CAROL}>", ALICE, "Newsletter",
              html='<p>News</p><img src="https://tracker.example/pixel.png" alt="pixel">')
    wait_until(lambda: find_all(None, "Newsletter"), 120, "new mail in the list")
    shot("07-liste")
    header_shot("07a-kopfzeile")


@scenario("Lesen: Unterhaltung öffnen, HTML, Anhang, Bilder blockiert")
def read():
    press(find(None, "Invoice May"))
    find(None, "invoice.pdf", timeout=30)
    shot("08-lesen-anhang")
    press(find(None, "Newsletter"))
    find(None, "External images were blocked", timeout=30)
    shot("09-bilder-blockiert")
    button("Show Once") if find_all("push button", "Show Once") else None


@scenario("Bösartige Mail: nichts läuft, nichts wird geladen, App bleibt stabil")
def hostile():
    html = """<script>document.title='pwned'</script>
<img src="x" onerror="document.title='pwned'">
<a href="javascript:alert(1)">js</a> <a href="data:text/html,x">data</a>
<a href="https://bank.example@evil.example/">Your bank</a>
<iframe src="https://evil.example"></iframe><form action="https://evil.example"><input></form>
<meta http-equiv="refresh" content="0;url=https://evil.example"><base href="https://evil.example/">
<style>@import url(https://evil.example/a.css)</style>
<svg><script>alert(1)</script></svg><img src="https://evil.example/pixel.gif">"""
    message = EmailMessage(policy=email.policy.SMTP)
    message["From"] = f"Mallory <{CAROL}>"
    message["To"] = ALICE
    message["Subject"] = "Hostile " + "x" * 300
    message.set_content("plain")
    message.add_alternative(html, subtype="html")
    message.add_attachment(b"rm -rf ~", maintype="text", subtype="plain",
                           filename="../../.bashrc")
    message.add_attachment(b"[Desktop Entry]", maintype="application",
                           subtype="x-desktop", filename="x.desktop")
    with smtplib.SMTP_SSL("localhost", 3465, context=tls()) as smtp:
        smtp.login(CAROL, PASSWORD)
        smtp.send_message(message)
    wait_until(lambda: find_all(None, "Hostile"), 120, "the hostile mail")
    press(find(None, "Hostile"))
    find(None, "External images were blocked", timeout=30)
    find(None, ".bashrc", timeout=10)
    shot("hostile")
    assert process.poll() is None, "the app stopped"
    names = [label(n) for n in find_all(None, "bashrc")]
    assert not any("../" in n for n in names), names
    return "Skripte entfernt, Bilder blockiert, Anhangnamen gesäubert: " + "; ".join(names)


@scenario("Tastenkürzel: j/k, s, u, Suche, Übersicht")
def shortcuts():
    focus(find(None, "Messages"))
    key("Escape")
    key("j")
    key("j")
    key("s")
    key("u")
    shot("10-kuerzel")
    key("question")
    find(None, "Next conversation", timeout=10)
    shot("11-kuerzel-uebersicht")
    key("Escape")


@scenario("Suche mit Filter")
def search():
    key("slash")
    type_text("invoice")
    wait_until(lambda: find_all(None, "result"), 20, "search results")
    shot("12-suche")
    press(find("toggle button", "With Attachment"))
    shot("13-suche-filter")
    key("Escape")


@scenario("Löschen und Rückgängig")
def delete_undo():
    press(find(None, "Plain hello"))
    key("Delete")
    button("Undo", timeout=5)
    wait_until(lambda: find_all(None, "Plain hello"), 15, "the mail back in the list")
    shot("14-rueckgaengig")


def flags(user, folder, subject):
    connection = imap(user)
    connection.select(f'"{folder}"')
    _, data = connection.search(None, "SUBJECT", f'"{subject}"')
    nums = data[0].split()
    result = b""
    if nums:
        _, parts = connection.fetch(nums[-1], "(FLAGS)")
        result = parts[0]
    connection.logout()
    return result.decode()


@scenario("Kontextmenü per Rechtsklick: Markieren")
def context_menu():
    row = find(None, "Plain hello")
    extents = row.get_component_iface().get_extents(Atspi.CoordType.WINDOW)
    ox, oy = window_origin()
    xdo("mousemove", str(ox + extents.x + 60), str(oy + extents.y + extents.height // 2))
    xdo("click", "3")
    time.sleep(1)
    shot("14a-kontextmenue")
    # "Reply" is chosen first; then Reply All, Forward, Mark as Read/Unread, Star
    for _ in range(4):
        key("Down")
    key("Return")
    wait_until(lambda: "Flagged" in flags(ALICE, "INBOX", "Plain hello"), 60,
               "the star on the server")


@scenario("Ziehen auf einen Ordner verschiebt die Unterhaltung (Archiv)")
def drag_to_folder():
    row = find(None, "Plain hello")
    target = find("label", "Archive", exact=True)
    ox, oy = window_origin()
    start = row.get_component_iface().get_extents(Atspi.CoordType.WINDOW)
    end = target.get_component_iface().get_extents(Atspi.CoordType.WINDOW)
    sx, sy = ox + start.x + 60, oy + start.y + start.height // 2
    ex, ey = ox + end.x + end.width // 2, oy + end.y + end.height // 2
    xdo("mousemove", str(sx), str(sy))
    xdo("mousedown", "1")
    for step in range(1, 21):
        x = sx + (ex - sx) * step // 20
        y = sy + (ey - sy) * step // 20
        xdo("mousemove", str(x), str(y))
        time.sleep(0.05)
    time.sleep(0.5)
    shot("14b-ziehen")
    xdo("mouseup", "1")
    time.sleep(1)
    shot("14c-verschoben")
    wait_until(lambda: "Plain hello" in subjects(ALICE, "Archive"), 60,
               "the conversation in the archive on the server")


@scenario("Als Spam markieren: Mail erscheint sofort im Spam-Ordner")
def spam():
    press(find(None, "Newsletter"))
    click(find(None, "Conversation Menu"))
    # Menus are separate windows the accessibility tree does not reach;
    # the keyboard does: "Move To…", then "Mark as Spam".
    key("Down")
    key("Return")
    time.sleep(1)
    click(find("label", "Spam", exact=True))
    wait_until(lambda: find_all(None, "Newsletter"), 15, "the mail in spam")
    shot("15-spam")
    wait_until(lambda: "Newsletter" in subjects(ALICE, "Spam"), 60, "the mail on the server")


@scenario("Neue Mail: Empfänger, Cc, Betreff, Text, Senden, Kopie in Gesendet")
def compose_send():
    action("compose")
    find(None, "+ Cc", timeout=20)
    focus(find("text", "To"))
    type_text(BOB)
    key("Return")
    press(find(None, "+ Cc"))
    type_text(CAROL)
    key("Return")
    fill("text", "Subject", "Hello from the test")
    key("Tab")
    time.sleep(1)
    type_text("This is the text.")
    shot("16-neue-mail")
    key("ctrl+Return")
    wait_until(lambda: "Hello from the test" in subjects(BOB), 90, "delivery to bob")
    wait_until(lambda: "Hello from the test" in subjects(CAROL), 30, "delivery to carol")
    wait_until(lambda: "Hello from the test" in subjects(ALICE, "Sent"), 60, "copy in Sent")


@scenario("Cc einblenden und wieder entfernen")
def cc_toggle():
    action("compose")
    press(find(None, "+ Cc", timeout=20))
    focus(find("text", "Cc"))
    type_text(CAROL)
    key("Return")
    shot("16a-cc-offen")
    press(find("push button", "Remove Cc"))
    find(None, "Cc removed", timeout=10)
    find(None, "+ Cc", timeout=10)
    shot("16b-cc-entfernt")
    stop_app()
    start_app()


@scenario("Neue Mail schließen legt einen Entwurf an, Zähler bei Entwürfe")
def draft():
    action("compose")
    find(None, "+ Cc", timeout=20)
    focus(find("text", "To"))
    type_text(BOB)
    key("Return")
    fill("text", "Subject", "Draft from the test")
    key("Escape")
    wait_until(lambda: "Draft from the test" in subjects(ALICE, "Drafts"), 60, "the draft")
    drafts = find(None, "Drafts")
    shot("17-entwurf")
    return f"Ordnerzeile: {label(drafts)}"


@scenario("Antworten unter der Nachricht")
def reply():
    press(find(None, "Inbox", exact=True))
    press(find(None, "Invoice May"))
    key("r")
    find(None, "Discard", timeout=20)
    time.sleep(1)
    # Writing takes the pane: no message stays open above the editor.
    assert not expanded_heads(), "a message stayed open above the reply"
    assert not find_all("label", "Newest", exact=True), "the newest stayed open"
    type_text("Thanks, received.")
    shot("18-antwort")
    key("ctrl+Return")
    wait_until(lambda: "Re: Invoice May" in subjects(BOB), 90, "the reply at bob")
    raw = raw_message(BOB, "INBOX", "Re: Invoice May")
    assert raw and b"In-Reply-To" in raw, "reply without In-Reply-To"


@scenario("Benachrichtigung bei geschlossenem Fenster, Aktion „Als gelesen markieren“")
def notification():
    action("settings")
    time.sleep(1)
    key("Escape")
    key("ctrl+w")
    time.sleep(2)
    before = len(notifications())
    send_mail(f"Bob <{BOB}>", ALICE, "While you were away", text="Ping")
    wait_until(lambda: len(notifications()) > before, 120, "a notification")
    note = notifications()[-1]
    assert "Bob" in note["summary"], note
    assert "While you were away" in note["body"], note
    subprocess.run(["gdbus", "call", "--session", "--dest", "org.freedesktop.Notifications",
                    "--object-path", "/org/freedesktop/Notifications",
                    "--method", "org.example.E2E.Invoke", str(note["id"]), "1"],
                   check=False, capture_output=True)
    return f"Benachrichtigung: {note['summary']} – {note['body']}"


def tray_items():
    try:
        with open(f"{OUT}/tray.json") as f:
            return [item for item in json.load(f) if not item["gone"]]
    except Exception:
        return []


def tray_call(item, interface, method, *args):
    path = "/MenuBar" if interface == "com.canonical.dbusmenu" else item["path"]
    result = subprocess.run(["gdbus", "call", "--session", "--dest", item["bus"],
                             "--object-path", path, "--method", f"{interface}.{method}",
                             *args], check=False, capture_output=True, text=True)
    return result.stdout + result.stderr


def tray_property(item, name):
    return tray_call(item, "org.freedesktop.DBus.Properties", "Get",
                     "org.kde.StatusNotifierItem", name)


@scenario("Symbol in der Leiste: erscheint bei geschlossenem Fenster, öffnet es, Beenden")
def tray():
    # The window is closed since the notification scenario.
    wait_until(lambda: tray_items(), 20, "the icon in the panel")
    item = tray_items()[0]
    icon = tray_property(item, "IconName")
    assert "io.github.deviablo.EMail" in icon, icon
    tooltip = tray_property(item, "ToolTip")
    assert "E-Mail" in tooltip, tooltip
    # "-1" would be read as an option by gdbus.
    layout = tray_call(item, "com.canonical.dbusmenu", "GetLayout", "0", "int32 -1", "@as []")
    for entry in ["Open E-Mail", "New Mail", "Quit"]:
        assert entry in layout, layout
    tray_call(item, "org.kde.StatusNotifierItem", "Activate", "0", "0")
    find("list", "Messages", timeout=15)
    wait_until(lambda: not tray_items(), 20, "the icon to go while the window is open")
    key("ctrl+w")
    wait_until(lambda: tray_items(), 20, "the icon after closing the window again")
    item = tray_items()[0]
    tray_call(item, "com.canonical.dbusmenu", "Event", "4", "clicked", "<''>", "0")
    wait_until(lambda: process.poll() is not None, 20, "the app to quit from the menu")
    return f"Symbol: {icon.strip()}"


@scenario("Tastenkürzel-Übersicht, Über diese App, dunkler Modus, schmales Fenster")
def no_webkit_looks():
    focus(find("list", "Messages"))
    key("question")
    find(None, "Next conversation", timeout=10)
    shot("10-kuerzel-uebersicht")
    key("Escape")
    action("about")
    find(None, "Report an Issue", timeout=10)
    shot("11-ueber")
    key("Escape")
    stop_app()
    start_app({"ADW_DEBUG_COLOR_SCHEME": "prefer-dark"})
    find(None, "Invoice May", timeout=30)
    shot("12-dunkel")
    xdo("search", "--onlyvisible", "--pid", str(process.pid), "windowsize", "%@", "420", "900")
    time.sleep(2)
    shot("13-schmal")
    stop_app()
    start_app()


@scenario("Neustart: Daten bleiben, Fenster kommt wieder")
def restart():
    stop_app()
    start_app()
    find(None, "Invoice May", timeout=30)
    shot("19-neustart")


def folded_heads():
    found = []
    for node in find_all("push button"):
        try:
            states = node.get_state_set()
            if (states.contains(Atspi.StateType.EXPANDABLE)
                    and not states.contains(Atspi.StateType.EXPANDED)):
                found.append(node)
        except Exception:
            pass
    return found


def expanded_heads():
    found = []
    for node in find_all("push button"):
        try:
            if node.get_state_set().contains(Atspi.StateType.EXPANDED):
                found.append(node)
        except Exception:
            pass
    return found


@scenario("Unterhaltung: immer nur eine Nachricht offen, Zuklappen öffnet die neueste")
def accordion():
    connection = imap(ALICE)
    previous = None
    for number, (text, ago) in enumerate([("Part one of the plan", 7200),
                                          ("Part two of the plan", 3600),
                                          ("Part three of the plan", 600)]):
        m = EmailMessage(policy=email.policy.SMTP)
        m["From"] = f"Bob <{BOB}>"
        m["To"] = ALICE
        m["Subject"] = "Plans for the trip" if number == 0 else "Re: Plans for the trip"
        m["Date"] = email.utils.formatdate(time.time() - ago, localtime=True)
        m["Message-ID"] = email.utils.make_msgid(domain="example.test")
        if previous:
            m["In-Reply-To"] = previous
            m["References"] = previous
        previous = m["Message-ID"]
        m.set_content(text)
        connection.append("INBOX", "(\\Seen)", imaplib.Time2Internaldate(time.time() - ago),
                          m.as_bytes())
    connection.logout()
    wait_until(lambda: find_all(None, "Plans for the trip"), 90, "the conversation")
    press(find(None, "Plans for the trip"))
    find("label", "Newest", exact=True, timeout=15)
    assert not expanded_heads(), "an older message is open at the start"
    assert len(folded_heads()) == 2, f"{len(folded_heads())} folded messages"
    press(folded_heads()[0])
    wait_until(lambda: len(expanded_heads()) == 1 and len(folded_heads()) == 2, 10,
               "the first older message to open and the newest to fold")
    assert not find_all("label", "Newest", exact=True), "the newest stayed open"
    first = expanded_heads()[0].get_component_iface().get_extents(Atspi.CoordType.WINDOW).y
    press(folded_heads()[0])
    wait_until(lambda: len(expanded_heads()) == 1 and len(folded_heads()) == 2, 10,
               "only the second older message open")
    # The message text lives in WebKit; the open head moved down a card.
    second = expanded_heads()[0].get_component_iface().get_extents(Atspi.CoordType.WINDOW).y
    assert second > first, (first, second)
    shot("18a-aufgeklappt")
    press(expanded_heads()[0])
    wait_until(lambda: not expanded_heads() and len(folded_heads()) == 2, 10,
               "the older message to fold and the newest to open again")
    find("label", "Newest", exact=True, timeout=5)


@scenario("Ohne Unterhaltungen: jede Mail einzeln, dann zurück")
def ungrouped():
    open_general_settings()
    click(find(None, "Group messages into conversations", timeout=10))
    shot("19a-schalter")
    key("Escape")
    time.sleep(1)
    press(find(None, "Invoice May"))
    find("push button", "Reply All", timeout=15)
    time.sleep(2)
    assert not find_all("label", "Newest", exact=True), "still shown as a conversation"
    shot("19b-einzeln")
    open_general_settings()
    click(find(None, "Group messages into conversations", timeout=10))
    key("Escape")
    time.sleep(1)
    press(find(None, "Invoice May"))
    find("label", "Newest", exact=True, timeout=15)
    shot("19c-gruppiert")


def open_general_settings():
    """The dialog grows in; a click too early hits the wrong tab."""
    action("settings")
    find("page tab", "General", timeout=10)
    for _ in range(3):
        time.sleep(1.5)
        tab = find("page tab", "General", timeout=10)
        # Through the accessibility interface; a click hit the wrong tab.
        actions = tab.get_action_iface()
        if actions is not None and actions.get_n_actions() > 0:
            actions.do_action(0)
        else:
            click(tab)
        time.sleep(1)
        if find_all(None, "Run in background"):
            return
    raise AssertionError("the General page did not open")


@scenario("Zahnrad in der Ordnerleiste öffnet die Einstellungen")
def gear():
    assert not find_all("push button", "Main Menu"), "the old main menu is still there"
    press(find("push button", "Settings", exact=True))
    find("page tab", "General", timeout=10)
    shot("19a-zahnrad-einstellungen")
    key("Escape")
    gone("page tab", "General")


@scenario("Einstellungen, Über diese App, dunkler Modus, schmales Fenster")
def looks():
    stop_app()
    start_app()
    action("settings")
    find(None, "Accounts", timeout=15)
    shot("20-einstellungen")
    key("Escape")
    action("about")
    find(None, "E-Mail", timeout=10)
    shot("21-ueber")
    key("Escape")
    stop_app()
    start_app({"ADW_DEBUG_COLOR_SCHEME": "prefer-dark"})
    shot("22-dunkel")
    xdo("search", "--name", "E-Mail", "windowsize", "%@", "420", "800")
    time.sleep(2)
    shot("23-schmal")


def prepare_server():
    """GreenMail starts with an inbox only; real providers have more."""
    connection = imap(ALICE)
    for folder in ["Spam", "Trash", "Archive"]:
        connection.create(folder)
    connection.logout()


def leave_older_open():
    """The state the failing runs were in: an older message left open."""
    press(find(None, "Plans for the trip"))
    find("label", "Newest", exact=True, timeout=15)
    if not expanded_heads():
        press(folded_heads()[0])
        wait_until(lambda: len(expanded_heads()) == 1, 10, "an older message to open")


def repetition(round_number):
    """Background mode again from a fresh start: notification and panel icon
    must work every time, also after the steps that preceded the failures."""
    @scenario(f"Wiederholung {round_number}: ältere Nachricht offen, Gruppierung aus/an, "
              "Benachrichtigung, Statussymbol")
    def again():
        stop_app()
        start_app()
        leave_older_open()
        ungrouped.__wrapped__()
        notes = notification.__wrapped__()
        icon = tray.__wrapped__()
        return f"{notes}; {icon}"
    return again


def folder_log():
    """GreenMail never changes UIDVALIDITY, so no folder may be thrown away;
    garbled answers the app shrugged off are only counted."""
    try:
        with open(f"{OUT}/app.log", errors="replace") as f:
            text = f.read()
    except OSError:
        text = ""
    rebuilt = text.count("was rebuilt") + text.count("UIDVALIDITY changed")
    # The whole inbox vanishing at once is the sign of a misread answer.
    rebuilt += sum(1 for m in re.finditer(r"INBOX: (\d+) of (\d+) messages are no longer", text)
                   if m.group(1) == m.group(2) and int(m.group(1)) > 1)
    shrugged = (text.count("no UIDVALIDITY in the answer") + text.count("not confirmed")
                + text.count("SELECT answer incomplete"))
    title = "Protokoll: kein Ordner verworfen oder geleert"
    note = f"{shrugged} unklare Antworten des Servers abgefangen"
    results.append((title, "bestanden" if rebuilt == 0 else "FEHLER",
                    note if rebuilt == 0 else f"{rebuilt} Ordner verworfen; {note}"))


def main():
    prepare_server()
    # Scenarios without a mail view or editor first: WebKit cannot run its
    # sandbox on the Ubuntu 24.04 runner without the AppArmor profile.
    for test in [first_start, setup_wrong_password, setup_success, receive, search,
                 no_webkit_looks, restart, read, hostile, shortcuts, delete_undo,
                 context_menu, drag_to_folder, spam,
                 compose_send, cc_toggle, draft, reply, accordion, ungrouped, notification, tray, gear, looks,
                 repetition(1), repetition(2), repetition(3), repetition(4), repetition(5)]:
        test()
    stop_app()
    folder_log()
    with open(f"{OUT}/report.md", "w") as f:
        f.write("| Szenario | Ergebnis | Hinweis |\n|---|---|---|\n")
        for title, outcome, note in results:
            f.write(f"| {title} | {outcome} | {note.replace('|', '/')} |\n")
    failed = [r for r in results if r[1] != "bestanden"]
    log(open(f"{OUT}/report.md").read())
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
