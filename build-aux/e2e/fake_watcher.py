#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Stands in for the panel during the end-to-end tests: accepts icons over
# the StatusNotifierItem protocol and records in a JSON file which are
# registered and which have gone again.
import json
import sys

import gi

gi.require_version("Gio", "2.0")
from gi.repository import Gio, GLib  # noqa: E402

LOG = sys.argv[1]
XML = """
<node>
  <interface name="org.kde.StatusNotifierWatcher">
    <method name="RegisterStatusNotifierItem"><arg type="s" direction="in"/></method>
    <method name="RegisterStatusNotifierHost"><arg type="s" direction="in"/></method>
    <property name="RegisteredStatusNotifierItems" type="as" access="read"/>
    <property name="IsStatusNotifierHostRegistered" type="b" access="read"/>
    <property name="ProtocolVersion" type="i" access="read"/>
    <signal name="StatusNotifierItemRegistered"><arg type="s"/></signal>
    <signal name="StatusNotifierItemUnregistered"><arg type="s"/></signal>
  </interface>
</node>
"""
items = []


def save():
    with open(LOG, "w") as f:
        json.dump(items, f, indent=1)


def vanished(conn, name, item):
    item["gone"] = True
    save()


def call(conn, sender, path, iface, method, params, invocation):
    if method == "RegisterStatusNotifierItem":
        (where,) = params.unpack()
        item = {"bus": sender, "path": where, "gone": False}
        items.append(item)
        save()
        Gio.bus_watch_name_on_connection(conn, sender, Gio.BusNameWatcherFlags.NONE,
                                         None, lambda c, n: vanished(c, n, item))
    invocation.return_value(None)


def get(conn, sender, path, iface, name):
    if name == "RegisteredStatusNotifierItems":
        return GLib.Variant("as", [i["bus"] + i["path"] for i in items if not i["gone"]])
    if name == "IsStatusNotifierHostRegistered":
        return GLib.Variant("b", True)
    return GLib.Variant("i", 0)


def acquired(conn, name):
    info = Gio.DBusNodeInfo.new_for_xml(XML)
    conn.register_object("/StatusNotifierWatcher", info.interfaces[0], call, get, None)
    save()


Gio.bus_own_name(Gio.BusType.SESSION, "org.kde.StatusNotifierWatcher",
                 Gio.BusNameOwnerFlags.NONE, acquired, None, None)
GLib.MainLoop().run()
