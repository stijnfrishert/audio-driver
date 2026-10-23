# audio-driver

## Features

- `realtime-dbus`: on Linux, promotes the audio thread to real-time scheduling through `rtkit` over D-Bus. Requires `libdbus-1-dev` (Debian/Ubuntu) or `dbus-devel` (Fedora) to build. Promotion is only attempted on direct hardware devices (`hw:`); the default device on a PipeWire or PulseAudio desktop is routed through a plugin and stays at normal priority.

On Windows and Android the audio thread is always promoted; on macOS CoreAudio already runs it at real-time priority.
