#!/usr/bin/env python3
"""Impersonate the bit of a desktop that a tray icon talks to.

A tray icon is not a drawing, it is a conversation: the application publishes a
StatusNotifierItem on the session bus, a panel reads the menu beside it and sends
clicks back. Nothing in `cargo test` can have that conversation — a GTK main loop
and a session bus are not things a unit test may own — so the smoke test runs the
real binary on a private bus and this script plays the panel:

  watch   Own `org.kde.StatusNotifierWatcher`, which is the name a panel is expected
          to hold. It matters that the name is owned: libayatana-appindicator only
          uses its session-bus path when a watcher says a host is registered, and
          falls back to a GTK status icon otherwise. Owning it makes the artifact
          take the path a modern desktop would give it. Every registration is
          printed as `REGISTERED <bus-name> <item-path>`.

  menu    Read the item's menu over `com.canonical.dbusmenu` — the interface panels
          actually use — print it, and optionally click items and wait for the menu
          to change afterwards. A click goes in the same way a panel's does
          (`Event(id, "clicked", …)`), so what comes out the far end is the
          application's own handling of a menu item, not a shortcut around it.

Usage:
  tray-probe.py watch
  tray-probe.py menu <bus-name> <item-path> [--brief] [--click TEXT | --expect TEXT]...

`--click` matches the first item whose label starts with TEXT (`Pause automatic
backups` — the toggle's label changes, so a prefix outlives it), and asks the bus
for a click. `--expect` waits until some item's label starts with TEXT, which is how
a click is shown to have arrived: the label only changes if the application handled
the click and republished its state. The two are applied in the order given, so
`--click Pause --expect Resume --click Quit` means what it reads like. `--brief`
skips the printed menu, for the calls whose job is to click rather than to show.

Exit codes: 0 success, 1 cannot talk to the bus, 2 usage, 3 an expectation never came
true, 4 no item matched a click.
"""

import sys
import time

import gi

gi.require_version("Gio", "2.0")
from gi.repository import Gio, GLib  # noqa: E402

WATCHER_NAME = "org.kde.StatusNotifierWatcher"
WATCHER_PATH = "/StatusNotifierWatcher"
MENU_IFACE = "com.canonical.dbusmenu"

# Only the members the tray crate's backend actually calls. A watcher that claims
# to host notifications (`IsStatusNotifierHostRegistered`) is what the indicator
# library waits for before it exports anything of its own.
WATCHER_XML = """
<node>
  <interface name="org.kde.StatusNotifierWatcher">
    <method name="RegisterStatusNotifierItem">
      <arg type="s" name="service" direction="in"/>
    </method>
    <method name="RegisterStatusNotifierHost">
      <arg type="s" name="service" direction="in"/>
    </method>
    <signal name="StatusNotifierItemRegistered">
      <arg type="s" name="service"/>
    </signal>
    <property name="RegisteredStatusNotifierItems" type="as" access="read"/>
    <property name="IsStatusNotifierHostRegistered" type="b" access="read"/>
    <property name="ProtocolVersion" type="i" access="read"/>
  </interface>
</node>
"""

TIMEOUT_MS = 10_000
POLL_SECONDS = 0.25


def session_bus():
    return Gio.bus_get_sync(Gio.BusType.SESSION, None)


def watch():
    """Own the watcher name and report every item that registers with it."""
    items = []

    def on_call(conn, sender, path, iface, method, params, invocation):
        if method == "RegisterStatusNotifierItem":
            # The argument is a bus name or an object path; a path belongs to the
            # sender, which is the only place it could possibly mean.
            argument = params.unpack()[0]
            service = sender if argument.startswith("/") else argument
            items.append((service, argument))
            print(f"REGISTERED {service} {argument}", flush=True)
            conn.emit_signal(
                None,
                path,
                iface,
                "StatusNotifierItemRegistered",
                GLib.Variant("(s)", (service,)),
            )
        else:
            print(f"CALL {method} {params.unpack()}", flush=True)
        invocation.return_value(None)

    def on_property(conn, sender, path, iface, name):
        if name == "IsStatusNotifierHostRegistered":
            return GLib.Variant("b", True)
        if name == "ProtocolVersion":
            return GLib.Variant("i", 0)
        if name == "RegisteredStatusNotifierItems":
            return GLib.Variant("as", [service for service, _ in items])
        return None

    interface = Gio.DBusNodeInfo.new_for_xml(WATCHER_XML).interfaces[0]

    def on_acquired(conn, name):
        conn.register_object(WATCHER_PATH, interface, on_call, on_property, None)
        # The smoke test waits for this line rather than guessing at a sleep.
        print("READY", flush=True)

    Gio.bus_own_name(
        Gio.BusType.SESSION,
        WATCHER_NAME,
        Gio.BusNameOwnerFlags.NONE,
        on_acquired,
        None,
        None,
    )
    GLib.MainLoop().run()
    return 0


def layout(bus, service, path):
    """The menu as a panel sees it: `GetLayout(0, -1, [])`, depth-first."""
    reply = bus.call_sync(
        service,
        path,
        MENU_IFACE,
        "GetLayout",
        GLib.Variant("(iias)", (0, -1, [])),
        GLib.VariantType("(u(ia{sv}av))"),
        Gio.DBusCallFlags.NONE,
        TIMEOUT_MS,
        None,
    )
    _, root = reply.unpack()
    entries = []
    _flatten(root, entries)
    return entries


def _flatten(node, entries):
    index, props, children = node
    entries.append(
        (
            index,
            str(props.get("label", "")),
            str(props.get("type", "")),
            bool(props.get("enabled", True)),
        )
    )
    for child in children:
        _flatten(child, entries)


def matches(entries, text):
    """Indexes whose label starts with `text`, ignoring label mnemonics (`_`)."""
    wanted = text.replace("_", "")
    return [
        index
        for index, label, _, _ in entries
        if label.replace("_", "").startswith(wanted)
    ]


def click(bus, service, path, index):
    # The reply type is left to the bus: dbusmenu implementations disagree about
    # whether `Event` returns a boolean or nothing at all.
    bus.call_sync(
        service,
        path,
        MENU_IFACE,
        "Event",
        GLib.Variant("(isvu)", (index, "clicked", GLib.Variant("av", []), 0)),
        None,
        Gio.DBusCallFlags.NONE,
        TIMEOUT_MS,
        None,
    )


def menu(argv):
    if len(argv) < 2:
        usage()
    service, path = argv[0], argv[1]

    actions = []
    brief = False
    rest = argv[2:]
    while rest:
        flag, _, value = rest[0].partition("=")
        if flag == "--brief" and not value:
            brief = True
            rest = rest[1:]
        elif flag in ("--click", "--expect") and value:
            actions.append((flag[2:], value))
            rest = rest[1:]
        elif flag in ("--click", "--expect") and rest[1:]:
            actions.append((flag[2:], rest[1]))
            rest = rest[2:]
        else:
            print(f"tray-probe: unexpected argument {rest[0]!r}", file=sys.stderr)
            usage()

    bus = session_bus()
    entries = layout(bus, service, path)
    if not brief:
        print(f"MENU {service} {path}", flush=True)
        for index, label, kind, enabled in entries:
            if kind == "separator":
                print(f"  #{index:<3} ------------------------------------", flush=True)
            else:
                print(
                    f"  #{index:<3} {label:<32} enabled={'yes' if enabled else 'no '}",
                    flush=True,
                )

    for action, value in actions:
        if action == "click":
            found = matches(entries, value)
            if not found:
                print(f"tray-probe: no menu item starts with {value!r}", file=sys.stderr)
                return 4
            if len(found) > 1:
                print(
                    f"tray-probe: {value!r} matches {len(found)} menu items",
                    file=sys.stderr,
                )
                return 4
            print(f"CLICK #{found[0]} {value!r}", flush=True)
            click(bus, service, path, found[0])
            # Give the click time to be handled before the next step reads the menu.
            time.sleep(POLL_SECONDS)
        else:
            if not wait_for(bus, service, path, value):
                print(f"tray-probe: no item ever started with {value!r}", file=sys.stderr)
                return 3
            print(f"EXPECTED {value!r}", flush=True)
    return 0


def wait_for(bus, service, path, text, timeout=TIMEOUT_MS / 1000):
    """Poll the menu until an item's label starts with `text`.

    A read can fail while the poll is running — clicking "Quit" takes the item, and
    with it the connection, off the bus — so a bus error is treated as "not yet".
    """
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            if matches(layout(bus, service, path), text):
                return True
        except GLib.Error:
            pass
        time.sleep(POLL_SECONDS)
    return False


def usage():
    print(__doc__.split("Usage:")[1].split("`--click`")[0].strip(), file=sys.stderr)
    sys.exit(2)


def main():
    argv = sys.argv[1:]
    if not argv:
        usage()
    if argv[0] == "watch":
        return watch()
    if argv[0] == "menu":
        return menu(argv[1:])
    usage()


if __name__ == "__main__":
    sys.exit(main())
