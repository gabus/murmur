// A GTK 4 test pad: one text view whose text is mirrored to a file, like `spike testpad`.
// GTK pastes through wl_data_device, which mutter has; the Slint pad's clipboard (arboard)
// needs a data-control protocol, which mutter lacks, so on GNOME it can't paste at all.
//
//   gjs -m tests/harness/gtk-testpad.js <file>
import GLib from 'gi://GLib';
import Gio from 'gi://Gio';
import Gtk from 'gi://Gtk?version=4.0';

const [out] = ARGV;
if (!out)
    throw new Error('usage: gjs -m gtk-testpad.js <file>');

const app = new Gtk.Application({flags: Gio.ApplicationFlags.NON_UNIQUE});
app.connect('activate', () => {
    const window = new Gtk.ApplicationWindow({
        application: app,
        title: 'murmur gtk testpad',
        default_width: 640,
        default_height: 240,
    });
    const view = new Gtk.TextView({wrap_mode: Gtk.WrapMode.WORD_CHAR});
    view.buffer.connect('changed', buffer => GLib.file_set_contents(out, buffer.text));
    window.set_child(view);
    window.present();
    view.grab_focus();
});
app.run([]);
