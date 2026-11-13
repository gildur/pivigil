# PiVigil

PiVigil turns a Raspberry Pi and camera into a video monitoring system. Watch a
space live and save recordings to review later.

- Record when motion is detected, or record continuously.
- Include footage from before and after motion events.
- Watch live video in a web browser or VLC.
- Save dated video clips to local or network storage.
- Add a camera label, date and time to the video.

See the [configuration reference](pivigil.toml) for available settings.

## Hardware

Only the configurations marked **Tested** below have been verified on real hardware.
The other configurations are expected to work but still need hardware testing.

| Raspberry Pi | Camera | Video encoding | Status |
| --- | --- | --- | --- |
| Zero 2 W | Camera Module 3 | Hardware | **Tested** |
| Zero 2 W | Camera Module 1 or 2 | Hardware | Expected; untested |
| Pi 3 Model B | Camera Module 2 | Hardware | **Tested** |
| Pi 3 | Camera Module 1 or 3 | Hardware | Expected; untested |
| Pi 4 | Camera Module 1, 2 or 3 | Hardware | Expected; untested |
| Pi 5 | Camera Module 1, 2 or 3 | Software | Expected; untested |

Hardware testing used 64-bit Raspberry Pi OS. Pi 5 recording performance still
needs verification. Other boards and camera models have not been assessed.

## Build

Build a Debian package on a Linux, macOS, or Windows machine with Docker and
Buildx installed.

From the repository root:

```sh
docker build --platform linux/arm64 -f deploy/Dockerfile.deb \
  --output type=local,dest=dist .
```

The package is written to `dist/pivigil_<version>-<revision>_arm64.deb`. It targets
**64-bit Raspberry Pi OS Bookworm** by default, using libraries from Raspberry
Pi's package repository. The version comes from `Cargo.toml`.

For a Pi running **64-bit Raspberry Pi OS Trixie**, select that release instead:

```sh
docker build --platform linux/arm64 -f deploy/Dockerfile.deb \
  --build-arg DEBIAN_RELEASE=trixie --output type=local,dest=dist .
```

Match the build release to the destination OS so the detected runtime library
dependencies are available. These packages do not target a 32-bit OS.

## Install and run

Copy the generated `.deb` from `dist/` to the Raspberry Pi. Stop other camera
applications, then run the following **on the Pi** (replace the filename with the
one produced):

```sh
sudo apt update
sudo apt install ./pivigil_1.0.0-1_arm64.deb
sudo journalctl -u pivigil.service -f
```

The service starts automatically after installation. Upgrades preserve your
configuration.

Edit `/etc/pivigil.toml`, then apply changes with
`sudo systemctl restart pivigil.service`.

Prepare any custom recording storage with the required mount and permissions
before starting the service.

## License

Copyright (c) 2026 Piotr Wolny. This project's source is licensed under
the [GNU General Public License version 3](LICENSE) (`GPL-3.0-only`).

The embedded NK57 Monospace font is dedicated to the public domain under CC0; its
[provenance note](assets/FONT-NOTICE.txt) accompanies deployment packages.

The bundled OpenH264 encoder includes its
[BSD license notice](assets/OPENH264-LICENSE.txt) in deployment packages.
