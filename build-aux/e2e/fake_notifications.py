#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Stands in for the desktop's notification service during the end-to-end
# tests: records every notification in a JSON file and can press a
# notification's action button on request (method Invoke).
import json
import sys

import gi

gi.require_version("Gio", "2.0")
from gi.repository import Gio, GLib  # noqa: E402

LOG = sys.argv[1]
XML = """
<node>
  <interface name="org.freedesktop.Notifications">
    <method name="GetCapabilities"><arg type="as" direction="out"/></method>
    <method name="Notify">
      <arg type="s" direction="in"/><arg type="u" direction="in"/>
      <arg type="s" direction="in"/><arg type="s" direction="in"/>
      <arg type="s" direction="in"/><arg type="as" direction="in"/>
      <arg type="a{sv}" direction="in"/><arg type="i" direction="in"/>
      <arg type="u" direction="out"/>
    </method>
    <method name="CloseNotification"><arg type="u" direction="in"/></method>
    <method name="GetServerInformation">
      <arg type="s" direction="out"/><arg type="s" direction="out"/>
      <arg type="s" direction="out"/><arg type="s" direction="out"/>
    </method>
    <signal name="ActionInvoked"><arg type="u"/><arg type="s"/></signal>
    <signal name="NotificationClosed"><arg type="u"/><arg type="u"/></signal>
  </interface>
  <interface name="org.example.E2E">
    <method name="Invoke"><arg type="u" direction="in"/><arg type="s" direction="in"/></method>
  </interface>
</node>
"""
notes = []
connection = None


def save():
    with open(LOG, "w") as f:
        json.dump(notes, f, indent=1)


def call(conn, sender, path, iface, method, params, invocation):
    if method == "GetCapabilities":
        invocation.return_value(GLib.Variant("(as)", (["actions", "body"],)))
    elif method == "GetServerInformation":
        invocation.return_value(GLib.Variant("(ssss)", ("e2e", "test", "1", "1.2")))
    elif method == "Notify":
        app, replaces, icon, summary, body, actions, hints, timeout = params.unpack()
        nid = replaces or len(notes) + 1
        notes.append({"id": nid, "summary": summary, "body": body, "actions": actions,
                      "closed": False})
        save()
        invocation.return_value(GLib.Variant("(u)", (nid,)))
    elif method == "CloseNotification":
        (nid,) = params.unpack()
        for note in notes:
            if note["id"] == nid:
                note["closed"] = True
        save()
        invocation.return_value(None)
    elif method == "Invoke":
        nid, key = params.unpack()
        conn.emit_signal(None, "/org/freedesktop/Notifications",
                         "org.freedesktop.Notifications", "ActionInvoked",
                         GLib.Variant("(us)", (nid, key)))
        invocation.return_value(None)


def acquired(conn, name):
    info = Gio.DBusNodeInfo.new_for_xml(XML)
    for iface in info.interfaces:
        conn.register_object("/org/freedesktop/Notifications", iface, call, None, None)
    save()


Gio.bus_own_name(Gio.BusType.SESSION, "org.freedesktop.Notifications",
                 Gio.BusNameOwnerFlags.NONE, acquired, None, None)
GLib.MainLoop().run()
