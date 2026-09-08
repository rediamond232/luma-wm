#!/usr/bin/env python3
"""Verify wlroots screencopy with the real grim client on a nested compositor."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
from importlib.util import module_from_spec, spec_from_file_location

from PIL import Image

ROOT = Path(__file__).resolve().parent.parent
spec = spec_from_file_location("nested", ROOT / "tools/check-nested.py")
nested = module_from_spec(spec)
spec.loader.exec_module(nested)


with tempfile.TemporaryDirectory(prefix="wm-screencopy-") as temporary:
    directory = Path(temporary)
    config = directory / "config.toml"
    config.write_text("[shell]\nenabled=false\n[theme]\nblur=false\nanimation_ms=0\n")
    socket = directory / "wm.sock"
    image_path = directory / "grim.png"
    region_path = directory / "region.png"
    cursor_path = directory / "cursor.png"
    dmabuf_path = directory / "dmabuf.txt"
    portal_receipt = directory / "portal.json"
    screencast_receipt = directory / "screencast.json"
    title = directory.name
    log_path = directory / "session.log"
    portal_config = directory / "xdg/xdg-desktop-portal-wlr/config"
    portal_config.parent.mkdir(parents=True)
    portal_config.write_text("[screencast]\nchooser_type=none\n")
    protocol_sources = []
    protocols = [
        (
            Path("/usr/share/wayland-protocols/staging/ext-image-capture-source/ext-image-capture-source-v1.xml"),
            "capture-source",
        ),
        (
            Path("/usr/share/wayland-protocols/staging/ext-image-copy-capture/ext-image-copy-capture-v1.xml"),
            "copy-capture",
        ),
        (
            Path("/usr/share/wayland-protocols/unstable/linux-dmabuf/linux-dmabuf-unstable-v1.xml"),
            "linux-dmabuf",
        ),
        (
            Path("/usr/share/wayland-protocols/staging/ext-foreign-toplevel-list/ext-foreign-toplevel-list-v1.xml"),
            "toplevel",
        ),
    ]
    for protocol, name in protocols:
        subprocess.run(
            ["wayland-scanner", "client-header", str(protocol), str(directory / f"{name}.h")],
            check=True,
        )
        source = directory / f"{name}.c"
        subprocess.run(["wayland-scanner", "private-code", str(protocol), str(source)], check=True)
        protocol_sources.append(str(source))
    dmabuf_client = directory / "dmabuf-capture-client"
    flags = subprocess.check_output(
        ["pkg-config", "--cflags", "--libs", "wayland-client", "gbm", "libdrm"], text=True
    ).split()
    subprocess.run(
        [
            "cc",
            "-std=c11",
            "-Wall",
            "-Wextra",
            "-Werror",
            f"-I{directory}",
            str(ROOT / "tools/dmabuf-capture-client.c"),
            *protocol_sources,
            *flags,
            "-o",
            str(dmabuf_client),
        ],
        check=True,
    )
    legacy_protocols = sorted(
        Path.home().glob(
            ".cargo/registry/src/*/wayland-protocols-wlr-*/wlr-protocols/unstable/"
            "wlr-screencopy-unstable-v1.xml"
        )
    )
    assert legacy_protocols, "wayland-protocols-wlr screencopy XML is unavailable"
    legacy_header = directory / "wlr-screencopy.h"
    legacy_source = directory / "wlr-screencopy.c"
    subprocess.run(
        ["wayland-scanner", "client-header", str(legacy_protocols[-1]), str(legacy_header)],
        check=True,
    )
    subprocess.run(
        ["wayland-scanner", "private-code", str(legacy_protocols[-1]), str(legacy_source)],
        check=True,
    )
    legacy_client = directory / "legacy-screencopy-client"
    legacy_flags = subprocess.check_output(
        ["pkg-config", "--cflags", "--libs", "wayland-client"], text=True
    ).split()
    subprocess.run(
        [
            "cc",
            "-std=c11",
            "-Wall",
            "-Wextra",
            "-Werror",
            f"-I{directory}",
            str(ROOT / "tools/legacy-screencopy-client.c"),
            str(legacy_source),
            *legacy_flags,
            "-o",
            str(legacy_client),
        ],
        check=True,
    )
    with log_path.open("w") as log:
        process = subprocess.Popen(
            [str(ROOT / "tools/run-nested.sh")],
            cwd=ROOT,
            env=dict(
                os.environ,
                WM_CONFIG=str(config),
                WM_SOCKET=str(socket),
                WM_NESTED_TITLE=title,
                XDG_CONFIG_HOME=str(directory / "xdg"),
                RUST_LOG="wm_compositor=debug,smithay=warn",
            ),
            stdout=log,
            stderr=log,
        )
        try:
            state = nested.wait_for(socket, lambda value: bool(value["outputs"]), process)
            output = state["outputs"][0]
            def capture(arguments, path):
                assert nested.request(socket, "exec " + json.dumps(["grim", *arguments, str(path)]))[
                    "ok"
                ]
                deadline = time.monotonic() + 10
                while not path.exists() or path.stat().st_size == 0:
                    assert process.poll() is None and time.monotonic() < deadline, log_path.read_text(
                        errors="replace"
                    )[-6000:]
                    time.sleep(0.05)

            capture(["-t", "png"], image_path)
            with Image.open(image_path) as source:
                image = source.convert("RGBA")
            assert image.size == (
                output["geometry"]["w"],
                output["geometry"]["h"],
            ), (image.size, output)
            assert image.getbbox() is not None
            assert image.getchannel("A").getextrema() == (255, 255)

            assert nested.request(
                socket, "exec " + json.dumps([str(dmabuf_client), str(dmabuf_path)])
            )["ok"]
            deadline = time.monotonic() + 10
            while not dmabuf_path.exists() or dmabuf_path.stat().st_size == 0:
                assert process.poll() is None and time.monotonic() < deadline, log_path.read_text(
                    errors="replace"
                )[-6000:]
                time.sleep(0.05)
            dmabuf_width, dmabuf_height, dmabuf_format, dmabuf_modifier = map(
                int, dmabuf_path.read_text().split()
            )
            assert (dmabuf_width, dmabuf_height) == image.size
            assert dmabuf_format > 0 and dmabuf_modifier >= 0
            assert "capturing output into DMA-BUF" in log_path.read_text(errors="replace")

            # A rootful private Xwayland forwards its pointer into the nested host.
            # Park it outside that host and let the cursor-leave redraw settle before
            # asserting that copy_with_damage remains pending on an idle output.
            if os.environ.get("WM_CAPTURE_PRIVATE_X11"):
                display_width, display_height = map(
                    int,
                    subprocess.check_output(
                        ["xdotool", "getdisplaygeometry"], text=True
                    ).split(),
                )
                subprocess.run(
                    ["xdotool", "mousemove", str(display_width - 1), str(display_height - 1)],
                    check=True,
                )
                time.sleep(0.5)

            legacy_path = directory / "legacy.ppm"
            legacy_waiting = directory / "legacy.waiting"
            legacy_generation = directory / "legacy.generation"
            legacy_finish = directory / "legacy.finish"
            assert nested.request(
                socket,
                "exec "
                + json.dumps(
                    [
                        str(legacy_client),
                        str(legacy_path),
                        str(legacy_waiting),
                        str(legacy_generation),
                        str(legacy_finish),
                    ]
                ),
            )["ok"]
            deadline = time.monotonic() + 10
            while not legacy_waiting.exists():
                assert process.poll() is None and time.monotonic() < deadline, log_path.read_text(
                    errors="replace"
                )[-6000:]
                time.sleep(0.05)
            config.write_text(config.read_text() + 'background="#152235"\n')
            assert nested.request(socket, "reload")["ok"]
            deadline = time.monotonic() + 10
            while True:
                assert process.poll() is None and time.monotonic() < deadline, log_path.read_text(
                    errors="replace"
                )[-6000:]
                try:
                    generation = int(legacy_generation.read_text())
                except (FileNotFoundError, ValueError):
                    time.sleep(0.05)
                    continue
                time.sleep(0.5)
                try:
                    if int(legacy_generation.read_text()) == generation:
                        legacy_finish.touch()
                        break
                except (FileNotFoundError, ValueError):
                    pass
            assert not legacy_path.exists(), "legacy copy_with_damage completed while idle"
            config.write_text(
                config.read_text().replace(
                    'background="#152235"', 'background="#1d2b3f"'
                )
            )
            assert nested.request(socket, "reload")["ok"]
            deadline = time.monotonic() + 10
            while not legacy_path.exists() or legacy_path.stat().st_size == 0:
                assert process.poll() is None and time.monotonic() < deadline, log_path.read_text(
                    errors="replace"
                )[-6000:]
                time.sleep(0.05)
            with Image.open(legacy_path) as source:
                legacy_image = source.convert("RGB")
            assert legacy_image.size == image.size
            assert legacy_image.getbbox() is not None
            assert legacy_image.tobytes() != image.convert("RGB").tobytes()
            updated_path = directory / "updated.png"
            capture(["-t", "png"], updated_path)
            with Image.open(updated_path) as source:
                image = source.convert("RGBA")

            region = (23, 17, 211, 137)
            capture(["-t", "png", "-g", f"{region[0]},{region[1]} {region[2]}x{region[3]}"], region_path)
            with Image.open(region_path) as source:
                clipped = source.convert("RGBA")
            assert clipped.size == region[2:]
            assert clipped.tobytes() == image.crop(
                (region[0], region[1], region[0] + region[2], region[1] + region[3])
            ).tobytes()

            wid = subprocess.check_output(
                ["xdotool", "search", "--name", "^" + title + "$"], text=True
            ).strip().splitlines()[-1]
            if os.environ.get("WM_CAPTURE_PRIVATE_X11"):
                subprocess.run(["xdotool", "windowraise", wid], check=True)
                subprocess.run(["xdotool", "windowfocus", "--sync", wid], check=True)
            else:
                subprocess.run(["xdotool", "windowactivate", "--sync", wid], check=True)
            subprocess.run(
                ["xdotool", "mousemove", "--window", wid, str(image.width // 2), str(image.height // 2)],
                check=True,
            )
            time.sleep(0.15)
            capture(["-t", "png", "-c"], cursor_path)
            with Image.open(cursor_path) as source:
                cursor_image = source.convert("RGBA")
            assert cursor_image.size == image.size
            assert cursor_image.tobytes() != image.tobytes(), "cursor-inclusive frame contains no cursor"

            assert nested.request(
                socket,
                "exec "
                + json.dumps(
                    ["/usr/bin/python3", str(ROOT / "tools/portal-screenshot.py"), str(portal_receipt)]
                ),
            )["ok"]
            deadline = time.monotonic() + 20
            while not portal_receipt.exists():
                assert process.poll() is None and time.monotonic() < deadline, log_path.read_text(
                    errors="replace"
                )[-8000:]
                time.sleep(0.05)
            portal = json.loads(portal_receipt.read_text())
            assert "error" not in portal, portal
            with Image.open(portal["path"]) as source:
                portal_image = source.convert("RGBA")
            assert portal_image.size == image.size
            assert portal_image.getchannel("A").getextrema() == (255, 255)

            time.sleep(0.5)
            assert nested.request(
                socket,
                "exec "
                + json.dumps(
                    [
                        "/usr/bin/python3",
                        str(ROOT / "tools/portal-screencast.py"),
                        str(screencast_receipt),
                        str(config),
                        "--dmabuf",
                    ]
                ),
            )["ok"]
            deadline = time.monotonic() + 25
            while not screencast_receipt.exists():
                assert process.poll() is None and time.monotonic() < deadline, log_path.read_text(
                    errors="replace"
                )[-10000:]
                time.sleep(0.05)
            screencast = json.loads(screencast_receipt.read_text())
            assert isinstance(screencast["node"], int) and screencast["node"] > 0, screencast
            assert len(screencast["frames"]) == 2, screencast
            with Image.open(screencast["frames"][0]) as source:
                first_streamed = source.convert("RGBA")
            with Image.open(screencast["frames"][-1]) as source:
                streamed = source.convert("RGBA")
            assert streamed.size == image.size, (streamed.size, image.size)
            assert streamed.getbbox() is not None
            assert first_streamed.tobytes() != streamed.tobytes()
            assert streamed.getpixel((streamed.width - 1, streamed.height - 1))[:3] == (38, 50, 71)
            assert log_path.read_text(errors="replace").count(
                "capturing output into DMA-BUF"
            ) >= 3, "portal stream did not negotiate DMA-BUF frames"
            print(
                "PASS: SHM pixels, a real DMA-BUF frame, and damage-paced legacy screencopy "
                "completed, plus clipped-region and cursor-inclusive opaque frames; "
                "xdg-desktop-portal returned a screenshot, and ScreenCast created PipeWire node "
                f"{screencast['node']} with two damage-paced frames at "
                f"{image.width}x{image.height}"
            )
        finally:
            Path(os.environ.get("WM_CHECK_SCREENCOPY_LOG", "/tmp/luma-screencopy.log")).write_text(
                log_path.read_text(errors="replace")
            )
            if process.poll() is None:
                nested.request(socket, "quit")
                process.wait(timeout=5)
