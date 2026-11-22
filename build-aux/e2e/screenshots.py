#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Takes the screenshots for the README and the metainfo: the installed app
# with a made-up but believable mailbox on the local test mail server.
# Only example domains and invented names appear.
# Usage (inside session.sh): screenshots.py OUTDIR
import email.policy
import email.utils
import imaplib
import os
import subprocess
import sys
import time
from email.message import EmailMessage

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import run as r  # noqa: E402

ME = "max.berger@example.org"
ME_NAME = "Max Berger"
HOUR = 3600
GERMAN = {"LANG": "de_DE.UTF-8", "LC_ALL": "de_DE.UTF-8", "LANGUAGE": "de"}
german = False


def t(english, deutsch):
    return deutsch if german else english
DAY = 24 * HOUR


def window_shot(name):
    """The active window alone, without the desktop around it."""
    found = subprocess.run(["xdotool", "getactivewindow"], capture_output=True, text=True)
    window = found.stdout.strip()
    target = ["-window", window] if window else ["-window", "root"]
    subprocess.run(["import", *target, f"{r.OUT}/{name}.png"], check=False)
    r.log(f"   screenshot {name}")


def message(sender, to, subject, text, ago, html=None, reply_to=None, attachment=None):
    m = EmailMessage(policy=email.policy.SMTP)
    m["From"] = sender
    m["To"] = to
    m["Subject"] = subject
    m["Date"] = email.utils.formatdate(time.time() - ago, localtime=True)
    domain = email.utils.parseaddr(sender)[1].split("@")[-1]
    m["Message-ID"] = email.utils.make_msgid(domain=domain)
    if reply_to is not None:
        m["In-Reply-To"] = reply_to["Message-ID"]
        m["References"] = " ".join(
            filter(None, [reply_to.get("References"), reply_to["Message-ID"]]))
    m.set_content(text)
    if html:
        m.add_alternative(html, subtype="html")
    if attachment:
        name, maintype, subtype, data = attachment
        m.add_attachment(data, maintype=maintype, subtype=subtype, filename=name)
    return m


def append(connection, folder, m, ago, seen=True, flagged=False):
    flags = " ".join(f for f, on in (("\\Seen", seen), ("\\Flagged", flagged)) if on)
    when = imaplib.Time2Internaldate(time.time() - ago)
    connection.append(folder, f"({flags})" if flags else None, when, m.as_bytes())


def photo():
    """A small, plain PNG as a holiday photo stand-in."""
    path = f"{r.OUT}/beach.png"
    subprocess.run(["convert", "-size", "640x400", "gradient:#7ec8e3-#f4e3b2", path],
                   check=False)
    try:
        with open(path, "rb") as f:
            return f.read()
    except OSError:
        return b"\x89PNG\r\n\x1a\n"


def fill_mailbox():
    me = f"{ME_NAME} <{ME}>"
    c = imaplib.IMAP4_SSL("localhost", 3993, ssl_context=r.tls())
    c.login(ME, r.PASSWORD)
    for folder in ["Sent", "Drafts", "Archive", "Spam", "Trash"]:
        c.create(folder)

    lena = "Lena Koch <lena.koch@example.org>"
    first = message(
        lena, me, "Handing over the flat on Saturday",
        "Hi Max,\n\nthe landlord would like to do the handover on Saturday. "
        "Could you bring the second key and the meter readings?\n\nThanks!\nLena",
        2 * DAY + 3 * HOUR)
    append(c, "INBOX", first, 2 * DAY + 3 * HOUR)
    answer = message(
        me, lena, "Re: Handing over the flat on Saturday",
        "Hi Lena,\n\nSaturday works for me. I'll bring the key and take photos "
        "of the meters on Friday.\n\nMax", DAY + 5 * HOUR, reply_to=first)
    append(c, "Sent", answer, DAY + 5 * HOUR)
    last = message(
        lena, me, "Re: Handing over the flat on Saturday",
        "Perfect, thank you!\n\nWe meet at 10 o'clock in front of the house. "
        "Afterwards we could have breakfast at the bakery on the corner.\n\n"
        "See you on Saturday\nLena",
        40 * 60,
        html="<p>Perfect, thank you!</p>"
             "<p>We meet at <b>10 o'clock</b> in front of the house. Please bring:</p>"
             "<ul><li>the second key</li><li>the meter readings</li>"
             "<li>the parking permit</li></ul>"
             "<p>Afterwards we could have breakfast at the bakery on the corner.</p>"
             "<p>See you on Saturday<br>Lena</p>",
        reply_to=answer)
    append(c, "INBOX", last, 40 * 60, seen=False)

    sophie = "Sophie Wagner <sophie.wagner@example.net>"
    cake = message(sophie, me, "Birthday dinner – who brings the cake?",
                   "Hi everyone,\n\nGrandpa turns 80 next Sunday! I booked a table "
                   "for twelve. Who can bring a cake?\n\nSophie", 3 * HOUR)
    append(c, "INBOX", cake, 3 * HOUR, seen=False)

    jonas = "Jonas Weber <jonas.weber@example.com>"
    append(c, "INBOX", message(
        jonas, me, "Photos from the weekend",
        "Hey Max,\n\nhere are the photos from the lake. The sunset one is my "
        "favourite.\n\nJonas", 5 * HOUR,
        attachment=("lake.png", "image", "png", photo())), 5 * HOUR, flagged=True)

    append(c, "INBOX", message(
        "Riverside Library <notices@library.example.org>", me,
        "Your loan ends in 3 days",
        "Dear reader,\n\nthe loan period for “The Long Way Home” ends on Friday. "
        "You can extend it once online.\n\nYour Riverside Library", DAY), DAY)

    append(c, "INBOX", message(
        "Travel Desk <bookings@travel.example.com>", me,
        "Your booking: Berlin → Hamburg, 14 March",
        "Thank you for your booking.\n\nDeparture 08:12, platform 7, coach 5, "
        "seat 42.\n\nHave a good trip!", DAY + 6 * HOUR), DAY + 6 * HOUR)

    paul = "Paul Meyer <paul.meyer@example.org>"
    bike = message(me, paul, "Bike still for sale?",
                   "Hi Paul,\n\nis the city bike still for sale?\n\nMax", 3 * DAY)
    append(c, "Sent", bike, 3 * DAY)
    append(c, "INBOX", message(
        paul, me, "Re: Bike still for sale?",
        "Yes, it is! You can try it out any evening this week.\n\nPaul",
        2 * DAY + 20 * HOUR, reply_to=bike), 2 * DAY + 20 * HOUR)

    append(c, "INBOX", message(
        "Community Garden <hello@garden.example.net>", me,
        "Spring planting this Saturday",
        "Dear gardeners,\n\nthe tomatoes and beans are waiting. We start at 9.\n\n"
        "Your garden team", 4 * DAY), 4 * DAY)
    c.logout()


def set_up_account():
    r.find(None, "Which address", timeout=30)
    time.sleep(1)
    window_shot("setup")
    # Tall enough that both server forms fit; pictures are taken later.
    r.xdo("search", "--name", "^E-Mail$", "windowsize", "%@", "1200", "1150")
    time.sleep(2)
    r.fill("text", "Email address", ME)
    r.button("Continue")
    r.fill("text", "Password", r.PASSWORD)
    r.button("Sign In")
    r.find(None, "Settings not found", timeout=120)
    r.press(r.find(None, "Enter Servers Yourself"))
    r.find(None, "Incoming mail (IMAP)", timeout=20)
    r.fill("text", "Password", r.PASSWORD)
    for node in r.find_all("text", "Server"):
        r.focus(node)
        r.key("ctrl+a")
        r.type_text("localhost")
    for node, port in zip(r.find_all("text", "Port"), ["3993", "3465"]):
        r.click(node, right=110)
        r.key("ctrl+a")
        r.type_text(port)
        r.key("Tab")
    r.button("Connect")
    r.find("text", "Your name", timeout=90)
    r.fill("text", "Your name", ME_NAME)
    r.fill("text", "Name for this account", "Personal")
    r.button("Done")
    r.find(None, "All set", timeout=30)
    r.button("Open Inbox")
    r.find(None, "Inbox", timeout=30)
    r.xdo("search", "--name", "^E-Mail$", "windowsize", "%@", "1200", "760")
    time.sleep(2)


def open_conversation():
    r.wait_until(lambda: r.find_all(None, "Handing over the flat"), 120, "the demo mail")
    r.wait_until(lambda: r.find_all(None, "Bike still for sale"), 60, "all demo mail")
    time.sleep(3)
    r.press(r.find(None, "Handing over the flat on Saturday"))
    r.find("label", t("Newest", "Neueste"), exact=True, timeout=20)
    time.sleep(4)
    # Away from the list, so no row shows a hover highlight.
    r.xdo("mousemove", "5", "5")
    time.sleep(1)


def set_signature():
    r.calm_down()
    r.action("settings")
    tab = r.find("page tab", "Signature", timeout=10)
    time.sleep(1.5)
    actions = tab.get_action_iface()
    if actions is not None and actions.get_n_actions() > 0:
        actions.do_action(0)
    else:
        r.click(tab)
    time.sleep(1)
    heading = r.find(None, "Signature for", timeout=10)
    # The editor is a web view below the heading and its explanation.
    extents = heading.get_component_iface().get_extents(r.Atspi.CoordType.WINDOW)
    ox, oy = r.window_origin()
    r.xdo("mousemove", str(ox + extents.x + extents.width // 2), str(oy + extents.y + 160))
    r.xdo("click", "1")
    time.sleep(1)
    r.type_text("Max Berger")
    r.key("shift+Return")
    r.type_text("Personal mail · sent with E-Mail")
    time.sleep(1)
    r.key("Escape")
    time.sleep(1)


def compose(name="compose"):
    r.action("compose")
    r.find(None, "+ Cc", timeout=20)
    r.focus(r.find("text", t("To", "An")))
    r.type_text("lena.koch@example.org")
    r.key("Return")
    r.fill("text", t("Subject", "Betreff"), "Saturday breakfast")
    r.key("Tab")
    time.sleep(1)
    r.type_text("Hi Lena, breakfast sounds great! I'll book a table for ")
    r.key("ctrl+b")
    r.type_text("11 o'clock")
    r.key("ctrl+b")
    r.type_text(".")
    r.key("Return")
    r.type_text("See you then")
    time.sleep(2)
    r.xdo("mousemove", "5", "5")
    window_shot(name)
    # Discard: close and delete the draft it leaves.
    r.key("Escape")
    time.sleep(2)


def step(name, function):
    try:
        function()
    except Exception as error:  # a missing picture must not hide the others
        r.log(f"   {name} failed: {error}")
        r.shot(f"FAIL-{name}")


def main():
    fill_mailbox()
    r.start_app()
    step("setup", set_up_account)

    def light():
        open_conversation()
        window_shot("main")
    step("main", light)
    step("signature", set_signature)
    step("compose", compose)

    def dark():
        r.stop_app()
        r.start_app({"ADW_DEBUG_COLOR_SCHEME": "prefer-dark"})
        open_conversation()
        window_shot("main-dark")
    step("dark", dark)
    # The same views in German, for checking that no text is cut off.
    global german
    german = True

    def deutsch():
        r.stop_app()
        r.start_app(GERMAN)
        open_conversation()
        window_shot("main-de")
        compose("compose-de")
        r.calm_down()
        r.action("add-account")
        r.find(None, "Welche Adresse", timeout=20)
        time.sleep(1)
        window_shot("setup-de")
        r.key("Escape")
        r.stop_app()
        r.start_app({**GERMAN, "ADW_DEBUG_COLOR_SCHEME": "prefer-dark"})
        open_conversation()
        window_shot("main-dark-de")
    step("german", deutsch)

    def icon_themes():
        """The list header with Adwaita and with Ubuntu's Yaru icons: the
        app's own symbols must look the same in both."""
        r.stop_app()
        r.start_app()
        r.header_shot("header-adwaita")
        settings = os.path.join(os.environ["XDG_CONFIG_HOME"], "gtk-4.0", "settings.ini")
        with open(settings) as f:
            original = f.read()
        with open(settings, "a") as f:
            f.write("gtk-icon-theme-name=Yaru\n")
        r.stop_app()
        r.start_app()
        r.header_shot("header-yaru")
        with open(settings, "w") as f:
            f.write(original)
    step("icon-themes", icon_themes)
    r.stop_app()
    missing = [n for n in ["setup", "main", "main-dark", "compose"]
               if not os.path.exists(f"{r.OUT}/{n}.png")]
    r.log(f"missing screenshots: {missing}" if missing else "all screenshots taken")
    sys.exit(1 if missing else 0)


if __name__ == "__main__":
    main()
