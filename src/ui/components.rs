//! Slint markup for the Setup (D-009) and Settings (D-010) windows.
//!
//! Fluent style, which follows the desktop's light/dark preference (read by Slint from the
//! `org.freedesktop.appearance color-scheme` portal setting). The components only display
//! properties and raise callbacks; `window.rs` owns all logic.

slint::slint! {
    #[style = "fluent"]
    import { Button, CheckBox, ComboBox, ProgressIndicator, Slider, Spinner, Palette } from "std-widgets.slint";

    // Re-exported so the offscreen renderer can force a colour scheme.
    export { Palette }

    export enum RowStatus { todo, busy, done, failed }

    export struct ModelChoice {
        id: string,
        name: string,
        summary: string,
        size: string,
        note: string,
        active: bool,
    }

    global Theme {
        out property <bool> dark: Palette.color-scheme == ColorScheme.dark;
        out property <color> success: dark ? #6ccb5f : #0f7b0f;
        out property <color> danger: dark ? #ff99a4 : #c42b1c;
        // Fluent's secondary text. Note: inside slint! a hex colour like #0000009e fails to lex
        // (Rust reads "9e" as a broken exponent).
        out property <color> muted: dark ? #ffffffc9 : #00000099;
        out property <color> card: dark ? #2b2b2b : #ffffff;
        out property <color> card-border: dark ? #ffffff17 : #0000001c;
        out property <color> divider: dark ? #ffffff12 : #00000012;
        out property <color> track: dark ? #ffffff24 : #0000001f;
        out property <color> keycap: dark ? #ffffff0f : #00000008;
        out property <color> hover: dark ? #ffffff0f : #0000000a;
        out property <color> popup: dark ? #2c2c2c : #fcfcfc;
        out property <color> on-status: dark ? #1c1c1c : #ffffff;
        out property <length> title-size: 22px;
        out property <length> small-size: 12px;
    }

    // Round status mark in front of a setup row: empty circle, spinner/progress ring, check, "!".
    component StatusIcon inherits Rectangle {
        in property <RowStatus> status;
        // Progress (0..1) drawn as a ring while busy; negative spins.
        in property <float> progress: -1;

        width: 22px;
        height: 22px;
        border-radius: 11px;
        background: root.status == RowStatus.done ? Theme.success
            : root.status == RowStatus.failed ? Theme.danger : transparent;
        border-width: root.status == RowStatus.todo || root.status == RowStatus.busy ? 2px : 0px;
        border-color: root.status == RowStatus.busy ? Theme.track : Theme.muted;
        accessible-role: text;
        accessible-label: root.status == RowStatus.done ? "Done"
            : root.status == RowStatus.failed ? "Problem"
            : root.status == RowStatus.busy ? "In progress" : "Not done";

        if root.status == RowStatus.done: Path {
            width: 100%;
            height: 100%;
            viewbox-width: 22;
            viewbox-height: 22;
            commands: "M 6.5 11.5 L 9.5 14.5 L 15.5 8.0";
            stroke: Theme.on-status;
            stroke-width: 2px;
            fill: transparent;
        }
        if root.status == RowStatus.failed: Path {
            width: 100%;
            height: 100%;
            viewbox-width: 22;
            viewbox-height: 22;
            commands: "M 11 5.5 L 11 12.5 M 11 15.0 L 11 16.6";
            stroke: Theme.on-status;
            stroke-width: 2.2px;
            fill: transparent;
        }
        if root.status == RowStatus.busy: Spinner {
            x: 1px;
            y: 1px;
            width: parent.width - 2px;
            height: parent.height - 2px;
            indeterminate: root.progress < 0;
            progress: max(root.progress, 0);
        }
    }

    // A key combination drawn as a key cap.
    component Keycap inherits Rectangle {
        in property <string> text;
        border-radius: 4px;
        border-width: 1px;
        border-color: Theme.card-border;
        background: Theme.keycap;
        HorizontalLayout {
            padding-left: 8px;
            padding-right: 8px;
            padding-top: 3px;
            padding-bottom: 3px;
            Text {
                text: root.text;
                font-size: 13px;
                font-weight: 600;
                color: Palette.foreground;
                vertical-alignment: center;
            }
        }
    }

    // Title with an optional line of secondary text under it.
    component RowText inherits VerticalLayout {
        in property <string> title;
        in property <string> detail;
        in property <bool> detail-error;
        in property <bool> strong: true;
        alignment: center;
        spacing: 2px;
        horizontal-stretch: 1;
        Text {
            text: root.title;
            font-weight: root.strong ? 600 : 400;
            color: Palette.foreground;
            wrap: word-wrap;
        }
        if root.detail != "": Text {
            text: root.detail;
            font-size: Theme.small-size;
            color: root.detail-error ? Theme.danger : Theme.muted;
            wrap: word-wrap;
        }
    }

    component Row inherits HorizontalLayout {
        padding-left: 16px;
        padding-right: 16px;
        padding-top: 12px;
        padding-bottom: 12px;
        spacing: 12px;
        cross-axis-alignment: center;
    }

    component Divider inherits Rectangle {
        height: 1px;
        background: Theme.divider;
    }

    component Card inherits Rectangle {
        background: Theme.card;
        border-radius: 8px;
        border-width: 1px;
        border-color: Theme.card-border;
        VerticalLayout {
            padding: 1px;
            @children
        }
    }

    component ShortcutRow inherits Row {
        in property <RowStatus> status;
        in property <string> trigger;
        in property <string> detail;
        in property <bool> detail-error;
        in property <bool> can-change;
        in property <bool> show-icon: true;
        in property <bool> strong: true;
        callback change();

        if root.show-icon: StatusIcon { status: root.status; }
        RowText {
            title: "Shortcut";
            detail: root.detail;
            detail-error: root.detail-error;
            strong: root.strong;
        }
        if root.trigger != "": Keycap { text: root.trigger; }
        if root.trigger == "" && root.status != RowStatus.failed: Text {
            text: "Not set";
            color: Theme.muted;
            vertical-alignment: center;
        }
        Button {
            text: "Change…";
            enabled: root.can-change;
            clicked => { root.change(); }
        }
    }

    // A labelled setting with its control(s) on the right.
    component SettingRow inherits Row {
        in property <string> label;
        in property <string> detail;
        in property <bool> detail-error;
        RowText {
            title: root.label;
            detail: root.detail;
            detail-error: root.detail-error;
            strong: false;
        }
        @children
    }

    export component SetupWindow inherits Window {
        title: "Murmur";
        // Fixed size that follows the content: Slint resizes the window when these change.
        min-width: 520px;
        max-width: 520px;
        min-height: main.preferred-height;
        max-height: main.preferred-height;
        background: Palette.background;
        default-font-size: 14px;

        in property <RowStatus> shortcut-status;
        in property <string> shortcut-trigger;
        in property <string> shortcut-detail;
        in property <bool> shortcut-detail-error;
        in property <bool> shortcut-can-change;

        in property <RowStatus> typing-status;
        in property <string> typing-detail;
        in property <bool> typing-detail-error;
        // Label of the typing button; "" hides it.
        in property <string> typing-button;

        in property <RowStatus> model-status;
        in property <float> model-ring: -1;
        in property <string> model-detail;
        in property <bool> model-detail-error;
        in property <bool> model-bar-visible;
        in property <float> model-bar-progress;
        in property <bool> model-bar-indeterminate;
        // Label of the Retry/Download button; "" hides it.
        in property <string> model-action;
        in property <[ModelChoice]> models;
        // Index of the model in use in `models`, or -1.
        in property <int> active-model: -1;
        // Keyboard highlight in the model list.
        property <int> list-highlight: -1;

        in-out property <bool> autostart;
        in property <string> autostart-error;
        in property <bool> done-enabled;
        in property <string> done-hint;

        // Natural height of the content at the current width (used for offscreen rendering).
        out property <length> content-height: main.preferred-height;

        callback change-shortcut();
        callback allow-typing();
        callback select-model(string);
        callback model-action-clicked();
        callback autostart-toggled(bool);
        callback done();

        // Opens the model list with the keyboard highlight on the model in use.
        public function show-model-list() {
            root.list-highlight = root.active-model;
            chooser.show();
        }

        function pick-model(index: int) {
            if (index >= 0 && index < root.models.length) {
                root.select-model(root.models[index].id);
            }
            chooser.close();
        }

        main := VerticalLayout {
            padding: 24px;
            spacing: 16px;

            VerticalLayout {
                spacing: 4px;
                Text {
                    text: "Welcome to Murmur";
                    font-size: Theme.title-size;
                    font-weight: 600;
                    color: Palette.foreground;
                }
                Text {
                    text: "Set up these three things once. After that, Murmur waits in the tray.";
                    color: Theme.muted;
                    wrap: word-wrap;
                }
            }

            Card {
                ShortcutRow {
                    status: root.shortcut-status;
                    trigger: root.shortcut-trigger;
                    detail: root.shortcut-detail;
                    detail-error: root.shortcut-detail-error;
                    can-change: root.shortcut-can-change;
                    change => { root.change-shortcut(); }
                }
                Divider { }
                Row {
                    StatusIcon { status: root.typing-status; }
                    RowText {
                        title: "Typing permission";
                        detail: root.typing-detail;
                        detail-error: root.typing-detail-error;
                    }
                    if root.typing-button != "": Button {
                        text: root.typing-button;
                        clicked => { root.allow-typing(); }
                    }
                }
                Divider { }
                Row {
                    StatusIcon {
                        status: root.model-status;
                        progress: root.model-ring;
                    }
                    VerticalLayout {
                        horizontal-stretch: 1;
                        spacing: 8px;
                        RowText {
                            title: "Speech model";
                            detail: root.model-detail;
                            detail-error: root.model-detail-error;
                        }
                        if root.model-bar-visible: ProgressIndicator {
                            progress: root.model-bar-progress;
                            indeterminate: root.model-bar-indeterminate;
                        }
                    }
                    if root.model-action != "": Button {
                        text: root.model-action;
                        clicked => { root.model-action-clicked(); }
                    }
                    choose := Button {
                        text: "Change…";
                        clicked => { root.show-model-list(); }
                    }
                }
            }

            VerticalLayout {
                spacing: 4px;
                CheckBox {
                    text: "Start Murmur when I log in";
                    checked <=> root.autostart;
                    toggled => { root.autostart-toggled(self.checked); }
                }
                if root.autostart-error != "": Text {
                    text: root.autostart-error;
                    font-size: Theme.small-size;
                    color: Theme.danger;
                    wrap: word-wrap;
                }
            }

            HorizontalLayout {
                spacing: 12px;
                cross-axis-alignment: center;
                Text {
                    text: root.done-hint;
                    font-size: Theme.small-size;
                    color: Theme.muted;
                    horizontal-alignment: right;
                    wrap: word-wrap;
                    horizontal-stretch: 1;
                }
                Button {
                    text: "Done";
                    primary: true;
                    enabled: root.done-enabled;
                    clicked => { root.done(); }
                }
            }
        }

        chooser := PopupWindow {
            width: 360px;
            // Right-aligned to the button; below it if there's room, else above it.
            property <length> below: choose.absolute-position.y + choose.height + 4px;
            property <length> above: choose.absolute-position.y - self.height - 4px;
            x: choose.absolute-position.x + choose.width - self.width;
            y: self.below + self.height <= root.height - 8px ? self.below : max(8px, self.above);
            close-policy: close-on-click-outside;
            forward-focus: chooser-keys;

            chooser-keys := FocusScope {
                key-pressed(event) => {
                    if (event.text == Key.DownArrow) {
                        root.list-highlight = Math.min(root.list-highlight + 1, root.models.length - 1);
                        return accept;
                    } else if (event.text == Key.UpArrow) {
                        root.list-highlight = Math.max(root.list-highlight - 1, 0);
                        return accept;
                    } else if (event.text == Key.Return || event.text == " ") {
                        root.pick-model(root.list-highlight);
                        return accept;
                    } else if (event.text == Key.Escape) {
                        chooser.close();
                        return accept;
                    }
                    reject
                }
                Rectangle {
                    background: Theme.popup;
                    border-radius: 8px;
                    border-width: 1px;
                    border-color: Theme.card-border;
                    drop-shadow-blur: 12px;
                    drop-shadow-offset-y: 4px;
                    drop-shadow-color: #00000040;
                    VerticalLayout {
                        padding: 4px;
                        spacing: 2px;
                        for m[i] in root.models: Rectangle {
                            border-radius: 6px;
                            background: item-touch.has-hover || i == root.list-highlight ? Theme.hover : transparent;
                            accessible-role: list-item;
                            accessible-label: m.name;
                            accessible-description: m.summary;
                            accessible-action-default => { root.pick-model(i); }
                            HorizontalLayout {
                                padding-left: 14px;
                                padding-right: 12px;
                                padding-top: 8px;
                                padding-bottom: 8px;
                                spacing: 12px;
                                cross-axis-alignment: center;
                                VerticalLayout {
                                    horizontal-stretch: 1;
                                    spacing: 2px;
                                    Text {
                                        text: m.name;
                                        font-weight: 600;
                                        color: Palette.foreground;
                                    }
                                    Text {
                                        text: m.summary;
                                        font-size: Theme.small-size;
                                        color: Theme.muted;
                                    }
                                }
                                VerticalLayout {
                                    spacing: 2px;
                                    Text {
                                        text: m.size;
                                        horizontal-alignment: right;
                                        color: Palette.foreground;
                                    }
                                    Text {
                                        text: m.note;
                                        font-size: Theme.small-size;
                                        horizontal-alignment: right;
                                        color: Theme.muted;
                                    }
                                }
                            }
                            // Accent mark on the model in use.
                            Rectangle {
                                x: 3px;
                                y: (parent.height - self.height) / 2;
                                width: 3px;
                                height: 16px;
                                border-radius: 1.5px;
                                background: Palette.accent-background;
                                visible: m.active;
                            }
                            item-touch := TouchArea {
                                clicked => { root.pick-model(i); }
                            }
                        }
                    }
                }
            }
        }
    }

    export component SettingsWindow inherits Window {
        title: "Murmur";
        min-width: 520px;
        max-width: 520px;
        min-height: main.preferred-height;
        max-height: main.preferred-height;
        background: Palette.background;
        default-font-size: 14px;

        in property <RowStatus> shortcut-status;
        in property <string> shortcut-trigger;
        in property <string> shortcut-detail;
        in property <bool> shortcut-detail-error;
        in property <bool> shortcut-can-change;

        in property <[string]> microphone-labels;
        in-out property <int> microphone-index;
        in property <[string]> model-labels;
        in-out property <int> model-index;

        in property <bool> download-visible;
        in property <string> download-text;
        in property <bool> download-error;
        in property <bool> download-bar-visible;
        in property <float> download-progress;
        in property <bool> download-indeterminate;
        in property <bool> download-retry;

        in-out property <float> pause-secs: 0.8;
        in property <[string]> idle-labels;
        in-out property <int> idle-index;
        in-out property <bool> autostart;
        in property <string> autostart-error;

        out property <length> content-height: main.preferred-height;

        // "0.8 s": tenths, so 1.0 doesn't print as "1".
        property <int> pause-tenths: Math.round(root.pause-secs * 10);
        property <string> pause-label: Math.floor(root.pause-tenths / 10) + "." + Math.mod(root.pause-tenths, 10) + " s";

        callback change-shortcut();
        callback microphone-selected(int);
        callback model-selected(int);
        callback retry-download();
        callback pause-released(float);
        callback idle-selected(int);
        callback autostart-toggled(bool);

        main := VerticalLayout {
            padding: 24px;
            spacing: 16px;

            Text {
                text: "Settings";
                font-size: Theme.title-size;
                font-weight: 600;
                color: Palette.foreground;
            }

            Card {
                ShortcutRow {
                    show-icon: false;
                    strong: false;
                    status: root.shortcut-status;
                    trigger: root.shortcut-trigger;
                    detail: root.shortcut-detail;
                    detail-error: root.shortcut-detail-error;
                    can-change: root.shortcut-can-change;
                    change => { root.change-shortcut(); }
                }
                Divider { }
                SettingRow {
                    label: "Microphone";
                    ComboBox {
                        width: 240px;
                        model: root.microphone-labels;
                        current-index <=> root.microphone-index;
                        selected => { root.microphone-selected(self.current-index); }
                    }
                }
                Divider { }
                Row {
                    VerticalLayout {
                        horizontal-stretch: 1;
                        spacing: 8px;
                        HorizontalLayout {
                            spacing: 12px;
                            cross-axis-alignment: center;
                            RowText {
                                title: "Speech model";
                                strong: false;
                            }
                            ComboBox {
                                width: 240px;
                                model: root.model-labels;
                                current-index <=> root.model-index;
                                selected => { root.model-selected(self.current-index); }
                            }
                        }
                        if root.download-visible: VerticalLayout {
                            spacing: 6px;
                            HorizontalLayout {
                                spacing: 12px;
                                cross-axis-alignment: center;
                                Text {
                                    text: root.download-text;
                                    font-size: Theme.small-size;
                                    color: root.download-error ? Theme.danger : Theme.muted;
                                    wrap: word-wrap;
                                    horizontal-stretch: 1;
                                }
                                if root.download-retry: Button {
                                    text: "Retry";
                                    clicked => { root.retry-download(); }
                                }
                            }
                            if root.download-bar-visible: ProgressIndicator {
                                progress: root.download-progress;
                                indeterminate: root.download-indeterminate;
                            }
                        }
                    }
                }
            }

            Card {
                SettingRow {
                    label: "Sentence ends after a pause of";
                    Slider {
                        width: 140px;
                        minimum: 0.4;
                        maximum: 2.0;
                        step: 0.1;
                        value <=> root.pause-secs;
                        released(v) => {
                            self.value = Math.round(v * 10) / 10;
                            root.pause-released(self.value);
                        }
                    }
                    Text {
                        text: root.pause-label;
                        min-width: 36px;
                        horizontal-alignment: right;
                        vertical-alignment: center;
                        color: Palette.foreground;
                    }
                }
                Divider { }
                SettingRow {
                    label: "Free memory after";
                    ComboBox {
                        width: 240px;
                        model: root.idle-labels;
                        current-index <=> root.idle-index;
                        selected => { root.idle-selected(self.current-index); }
                    }
                }
                Divider { }
                Row {
                    VerticalLayout {
                        horizontal-stretch: 1;
                        spacing: 4px;
                        CheckBox {
                            text: "Start Murmur when I log in";
                            checked <=> root.autostart;
                            toggled => { root.autostart-toggled(self.checked); }
                        }
                        if root.autostart-error != "": Text {
                            text: root.autostart-error;
                            font-size: Theme.small-size;
                            color: Theme.danger;
                            wrap: word-wrap;
                        }
                    }
                }
            }
        }
    }
}
