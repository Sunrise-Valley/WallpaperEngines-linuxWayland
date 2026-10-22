# Building and running

## Requirements

**Rust** — 1.85 or newer (the UI crate uses edition 2024). Stable toolchain is fine.

**System libraries**

| Need | Debian/Ubuntu package | Used by |
|---|---|---|
| Wayland client + protocols | `libwayland-dev` | kernel |
| EGL / GLES 3 driver | `libegl1-mesa-dev`, `libgles2-mesa-dev` | renderer |
| ffmpeg headers (decode helper) | `libavcodec-dev libavformat-dev libavutil-dev libdrm-dev` | renderer |
| `gcc`, `ar` | `build-essential` | renderer's `build.rs` |
| XKB, Vulkan, udev, OpenSSL | `libxkbcommon-dev libvulkan-dev libudev-dev libssl-dev` | UI (libcosmic) |

`scripts/install-dev-deps.sh` installs the UI-side set with `apt-get`. The ffmpeg headers
are separate because the renderer links against the *sonames* the system actually ships
(`libavformat.so.63`, `libavcodec.so.63`, `libavutil.so.61` on ffmpeg 7.x).

### Why the renderer needs ffmpeg headers

`crates/we-scene-render/c` contains one C file, `av_decode.c`: the in-process VAAPI decode
path used by the zero-copy video mode. It is compiled by `build.rs` against the real
ffmpeg headers rather than hand-written FFI, because `AVCodecContext` field layout changes
between ffmpeg releases.

If the headers are not in the default prefix, point `FFMPEG_DEV_ROOT` at a prefix that
contains `usr/include/x86_64-linux-gnu/libavcodec/avcodec.h` and
`usr/lib/x86_64-linux-gnu/libav*.so*`:

```sh
# without root: unpack the -dev .debs into a local prefix
apt-get download libavcodec-dev libavformat-dev libavutil-dev libdrm-dev
mkdir -p ~/.local/opt/ffmpeg-dev
for f in ./*.deb; do dpkg -x "$f" ~/.local/opt/ffmpeg-dev/; done
export FFMPEG_DEV_ROOT=~/.local/opt/ffmpeg-dev
```

`scripts/make-dev-sysroot.sh` does the same trick for the libcosmic headers if you cannot
install packages system-wide.

---

## Build

Each crate is standalone (own `Cargo.lock`), so pick the one you need:

```sh
cargo build --release --manifest-path crates/we-scene/Cargo.toml          # parser, no system deps
cargo build --release --manifest-path crates/we-scene-render/Cargo.toml   # renderer + offline CLI
cargo build --release --manifest-path crates/we-native/Cargo.toml         # kernel
cargo build --release --manifest-path crates/wallpaper-hub/Cargo.toml     # COSMIC app (libcosmic from git)
```

The first `wallpaper-hub` build fetches `libcosmic` from git; everything else comes from
crates.io. For an offline build you need a populated cargo cache:

```sh
cargo build --offline --manifest-path crates/we-native/Cargo.toml
```

`cargo check --offline` passes for all four crates when the dependency cache is already
populated: `we-scene`, `we-scene-render` and `we-native` need only crates.io packages;
`wallpaper-hub` additionally needs the `libcosmic` git dependency in `~/.cargo/git` (or a
`[patch]` pointing at a local checkout). On a cold machine the first build needs network
access for that git dependency.

The crates are independent (each has its own `Cargo.lock`), so build them one at a time as
shown above; there is no workspace-level `Cargo.toml` that would unify them.

---

## Run

### Kernel + CLI

```sh
export WE_FFMPEG=/usr/bin/ffmpeg          # only needed if ffmpeg is not on PATH
./target/release/we-native --serve "$XDG_RUNTIME_DIR/we-native.sock"

# in another terminal
./scripts/we-ctl status
./scripts/we-ctl apply-scene ~/Templates/<id>      # or just <id>: the kernel looks it up in the library roots
./scripts/we-ctl pause
./scripts/we-ctl quit
```

Kernel flags:

| Flag | Purpose |
|---|---|
| `--serve <socket>` | stay resident, accept control commands |
| `--globals` | list the compositor's advertised globals (protocol availability) |
| `--list-outputs` | list outputs and exit |
| `--image <file>` / `--video <file>` / `--scene <dir>` | play one wallpaper directly |
| `--seconds N` | exit after N seconds (smoke test) |
| `--stats` | report cpu time and RSS every 5 s |
| `--track-only` | track toplevel state without creating any surface |
| `--sleep-on-maximized` | treat maximized windows as "covered" too |
| `--video-probe <file>` | decode-only probe (no surface) |
| `--video-max WxH` | cap decode size |
| `--size WxH`, `--frames N` | probe parameters |

### Offline single-frame render

```sh
./target/release/we-scene-render <wallpaper dir> out.png 1600 \
    --size 2560x1440 --t 12.5 --mouse 0.4,0.6 --prop MyText="hello"
```

Useful flags: `--frames N` (advance N frames at a fixed 1/30 s, keep the last), `--t`
(freeze at a scene time), `--mouse X,Y` (normalized pointer), `--prop name=value`
(user properties), `--bench N` / `--opbench N` (CPU and per-GL-call microbenchmarks),
`--dt` (fixed-step override, to simulate slow frames).

The CLI pins the wall clock to `2025-01-01T00:00:00Z + t`; override with
`WE_NOW_EPOCH=<unix seconds>` if a scene renders date text and you need to match a
reference frame.

### GUI

```sh
./target/release/wallpaper-hub
./target/release/wallpaper-hub --page discover      # wallpapers | discover | settings
./target/release/wallpaper-hub --select-id <id>     # preselect one wallpaper
```

The UI drives the kernel over the socket; start the kernel first (or install
`packaging/we-native.service`).

### Per-subsystem logging

The renderer and kernel have one convention for diagnostic logging:
`WE_DEBUG_<SUBSYSTEM>=1` prints what that subsystem decided this frame
(`PARTICLES`, `LAYERS`, `UV`, `PASSES`, `LINKS`, `TEX`, `BOIDS`, `LIGHTS`, `ATTACH`,
`MVP`, `FBO`, …). Unset means the code path does not execute at all, so leaving them in
costs nothing on the hot path. They are meant for bug reports: include the output of the
subsystem you think is misbehaving.

### systemd user service

```sh
mkdir -p ~/.config/systemd/user
cp packaging/we-native.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now we-native.service
```

---

## Tests

```sh
for c in we-scene we-scene-render we-native; do
    cargo test --manifest-path crates/$c/Cargo.toml
done
```

Most tests are self-contained. One renderer test needs a directory of dumped effect
shaders and is skipped unless `WE_FUSE_DUMP_DIR` is set.

---

## Troubleshooting

**`error: linking with cc failed` / missing `libav*.so`** — the renderer links ffmpeg by
soname. Install the runtime packages for your ffmpeg version, or adjust the
`rustc-link-lib=dylib:+verbatim=` lines in `crates/we-scene-render/build.rs` to the
sonames your distro ships (`ls /usr/lib/x86_64-linux-gnu/libavcodec.so.*`).

**`连不上 Wayland`** — `WAYLAND_DISPLAY` is unset or points at another session. The kernel
must run inside your graphical session.

**A wallpaper stays black** — try `WE_EFFECTS=off` first (isolates the effect chain), then
`we-scene-render <dir> out.png --size <screen>` to see whether the scene renders offline
at all. If offline is fine and live is not, the difference is in the surface/blit path, not
the scene.

**High CPU with nothing visible** — check `--stats` output and whether a fullscreen window
is being reported as such by the compositor; `WE_SLEEP_MODE=fullscreen` is the default
policy but relies on the compositor's toplevel list.
