#!/usr/bin/env python3
"""A stand-in for gvfs's MTP volume monitor, for the live gvfs test.

It owns org.gtk.vfs.MTPVolumeMonitor on the session bus, answers List
with one phone (a Pixel 8 at mtp://Google_Pixel_8_X/), and emits
VolumeAdded every 300ms so a watcher sees a signal. Run it inside a
private bus, never the desktop's:

    dbus-run-session -- sh -c 'scripts/fake-mtp-monitor.py & sleep 1;
        ROBIN_GVFS_LIVE=1 cargo test --lib gvfs::tests::live -- --ignored'
"""
from gi.repository import Gio, GLib

BUS = "org.gtk.vfs.MTPVolumeMonitor"
PATH = "/org/gtk/Private/RemoteVolumeMonitor"
VOL = "(ssssssbbssa{ss}sa{sv})"
XML = f"""
<node><interface name="org.gtk.Private.RemoteVolumeMonitor">
  <method name="List">
    <arg type="a(ssssbbbbbbbbuasa{{ss}}sa{{sv}})" direction="out"/>
    <arg type="a{VOL}" direction="out"/>
    <arg type="a(ssssssbsassa{{sv}})" direction="out"/>
  </method>
  <signal name="VolumeAdded">
    <arg type="s"/><arg type="s"/><arg type="{VOL}"/>
  </signal>
</interface></node>"""

volume = ("0x1", "Pixel 8", ". GThemedIcon phone", "", "",
          "mtp://Google_Pixel_8_X/", True, False, "", "", {},
          "gvfs.time_detected_usec.1", {})


def call(conn, sender, path, iface, method, params, inv):
    inv.return_value(GLib.Variant(
        f"(a(ssssbbbbbbbbuasa{{ss}}sa{{sv}})a{VOL}a(ssssssbsassa{{sv}}))",
        ([], [volume], [])))


def acquired(conn, name):
    info = Gio.DBusNodeInfo.new_for_xml(XML).interfaces[0]
    conn.register_object(PATH, info, call, None, None)

    def ping():
        conn.emit_signal(None, PATH, "org.gtk.Private.RemoteVolumeMonitor",
                         "VolumeAdded",
                         GLib.Variant(f"(ss{VOL})", (BUS, "0x1", volume)))
        return True
    GLib.timeout_add(300, ping)


Gio.bus_own_name(Gio.BusType.SESSION, BUS, 0, acquired, None, None)
GLib.MainLoop().run()
