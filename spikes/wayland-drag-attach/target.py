#!/usr/bin/env python3
import gi, json, sys, time, hashlib, os
gi.require_version("Gtk", "4.0")
gi.require_version("Gdk", "4.0")
from gi.repository import Gtk, Gdk, GLib

RESULT = "/tmp/spike-target-result.json"
log = open("/tmp/spike-target.log", "a", buffering=1)

def note(msg):
    log.write(f"{time.strftime('%H:%M:%S')} {msg}\n")

def main():
    if os.path.exists(RESULT):
        os.remove(RESULT)
    app = Gtk.Application(application_id="dev.splice.spike.target")

    def activate(app):
        win = Gtk.Window(application=app, title="Splice spike drop target")
        label = Gtk.Label(label="SPIKE DROP TARGET — waiting for a drop")
        label.set_css_classes(["title-1"])
        win.set_child(label)
        target = Gtk.DropTarget.new(Gdk.FileList, Gdk.DragAction.COPY)

        def on_enter(t, x, y):
            note(f"dnd enter at {x:.0f},{y:.0f} formats={t.get_formats().to_string() if t.get_formats() else None}")
            return Gdk.DragAction.COPY

        def on_motion(t, x, y):
            return Gdk.DragAction.COPY

        def on_drop(t, value, x, y):
            files = value.get_files()
            out = []
            for f in files:
                path = f.get_path()
                entry = {"uri": f.get_uri(), "path": path}
                try:
                    with open(path, "rb") as fh:
                        data = fh.read()
                    entry["bytes"] = len(data)
                    entry["sha256"] = hashlib.sha256(data).hexdigest()
                except Exception as e:
                    entry["error"] = repr(e)
                out.append(entry)
            note(f"drop at {x:.0f},{y:.0f}: {json.dumps(out)}")
            with open(RESULT, "w") as fh:
                json.dump({"drop_at": [x, y], "files": out}, fh)
            label.set_label("DROP RECEIVED")
            GLib.timeout_add(800, lambda: (app.quit(), False)[1])
            return True

        target.connect("enter", on_enter)
        target.connect("motion", on_motion)
        target.connect("drop", on_drop)
        win.add_controller(target)
        win.connect("notify::is-active", lambda w, _p: note(f"is-active={w.is_active()}"))
        if os.environ.get('SPIKE_TARGET_MAXIMIZE'):
            win.maximize()
        else:
            win.fullscreen()
        win.present()
        note("target window presented (fullscreen)")
        GLib.timeout_add_seconds(40, lambda: (note("target timeout"), app.quit(), False)[2])

    app.connect("activate", activate)
    app.run(None)

if __name__ == "__main__":
    main()
