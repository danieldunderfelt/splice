#!/usr/bin/env python3
import gi, os, sys, time
gi.require_version("Gtk", "4.0")
gi.require_version("Gdk", "4.0")
from gi.repository import Gtk, Gdk, Gio, GLib

log = open("/tmp/spike-source.log", "a", buffering=1)

def note(msg):
    log.write(f"{time.strftime('%H:%M:%S')} {msg}\n")

def main():
    files = [Gio.File.new_for_path(os.path.abspath(p)) for p in sys.argv[1:]]
    app = Gtk.Application(application_id="dev.splice.spike.source")

    def activate(app):
        win = Gtk.Window(application=app, title="Splice spike drag source")
        label = Gtk.Label(label="SPIKE DRAG SOURCE — press and drag anywhere")
        label.set_css_classes(["title-1"])
        win.set_child(label)
        source = Gtk.DragSource.new()
        source.set_actions(Gdk.DragAction.COPY)

        def on_prepare(s, x, y):
            note(f"prepare at {x:.0f},{y:.0f}")
            return Gdk.ContentProvider.new_for_value(Gdk.FileList.new_from_list(files))

        def on_begin(s, drag):
            note("drag-begin")

        def on_end(s, drag, delete_data):
            note(f"drag-end selected_action={drag.get_selected_action()} delete={delete_data}")
            GLib.timeout_add(800, lambda: (app.quit(), False)[1])

        def on_cancel(s, drag, reason):
            note(f"drag-cancel reason={reason}")
            return False

        source.connect("prepare", on_prepare)
        source.connect("drag-begin", on_begin)
        source.connect("drag-end", on_end)
        source.connect("drag-cancel", on_cancel)
        win.add_controller(source)
        motion = Gtk.EventControllerMotion.new()
        motion.connect("motion", lambda c, x, y: note(f"motion {x:.0f},{y:.0f}"))
        win.add_controller(motion)
        if os.environ.get("SPIKE_SOURCE_WINDOWED"):
            win.set_default_size(700, 500)
        else:
            win.fullscreen()
        win.present()
        note("source window presented (fullscreen)")
        GLib.timeout_add_seconds(40, lambda: (note("source timeout"), app.quit(), False)[2])

    app.connect("activate", activate)
    app.run(None)

if __name__ == "__main__":
    main()
