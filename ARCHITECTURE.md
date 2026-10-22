# Architecture

This document describes how Wallpaper Hub is put together: the three blocks, the control
plane between them, the renderer's pass model, and the failure-handling machinery that
keeps a broken wallpaper from taking the desktop with it.

Everything here is implemented in this repository. There is no Wine, no D3D translation
layer, no embedded browser.

---

## 1. The three blocks

```
 ┌────────────────────────────────────────────────────────────────────────┐
 │ UI — crates/wallpaper-hub                                              │
 │                                                                        │
 │  libcosmic application (COSMIC / iced)                                 │
 │   · library scan: local library roots (+ optional Steam dir) → list    │
 │   · workshop metadata browsing (thumbnails, search, paging)            │
 │   · detail panel: author user-properties (bool/color/text/combo/slider)│
 │   · settings: per-scene fps, render scale, effects, sleep policy       │
 │   · all state changes go out as control-plane commands                 │
 └───────────────────────────────┬────────────────────────────────────────┘
                                 │  unix stream socket
                                 │  $XDG_RUNTIME_DIR/we-native.sock
                                 │  request:  one JSON object + "\n"
                                 │  response: one JSON object + "\n"
 ┌───────────────────────────────▼────────────────────────────────────────┐
 │ KERNEL — crates/we-native                                              │
 │                                                                        │
 │  main loop (calloop, one Wayland event queue)                          │
 │   · one zwlr_layer_shell_v1 surface per wl_output, background layer    │
 │   · viewporter: render at physical size, advertise logical size        │
 │   · wl_pointer (on our surface) + ext_image_copy_capture cursor        │
 │     session (global pointer) → renderer.set_mouse()                     │
 │   · control socket thread → apply/pause/quit/set/set-prop/mouse/audio  │
 │   · video wallpapers: ffmpeg child process, SIGSTOP when not visible   │
 │   · audio: PipeWire/Pulse sink enumeration + 16-band spectrum          │
 │   · guards: watchdog, display_guard, commit_guard, slowloop            │
 └───────────────────────────────┬────────────────────────────────────────┘
                                 │  direct Rust calls (linked as a library)
 ┌───────────────────────────────▼────────────────────────────────────────┐
 │ RENDERER — crates/we-scene-render                                      │
 │                                                                        │
 │  SceneRenderer::open(dir, w, h)                                        │
 │   · we_scene::pkg  → PKGV entries (scene.json, textures, effects)      │
 │   · we_scene::tex  → TEXV/TEXI/TEXB → decoded RGBA / DXT              │
 │   · we_scene::scene→ objects, camera, general, effects, scripts        │
 │   · compile: objects → passes (target RT, shader, uniforms, blend)     │
 │   · per frame: advance clocks → resolve uniforms → draw pass 0..N      │
 │   → screen FBO, or → dmabuf for the zero-copy path                     │
 └───────────────────────────────┬────────────────────────────────────────┘
                                 │
                    wl_surface (layer-shell background)
                                 │
                          compositor → display
```

The UI never talks to GL or Wayland directly. The kernel never parses `scene.pkg`
itself — it hands the wallpaper directory to the renderer. The renderer knows nothing
about surfaces; it draws into whatever FBO / dmabuf it is given. That split is what makes
the offline single-frame CLI possible: same renderer, same scene, no compositor.

---

## 2. Control plane

A line-oriented JSON protocol over a Unix stream socket. One request object per line, one
response object per line, no framing beyond the newline. The parser is deliberately
tolerant (it reads the fields it needs and ignores the rest) so that an older UI and a
newer kernel can still talk.

Requests:

| Request | Meaning |
|---|---|
| `{"cmd":"status"}` | report what is playing, frame counts, cpu, rss |
| `{"cmd":"apply","kind":"scene\|video\|image","path":"<dir, file or wallpaper id>"}` | load and play |
| `{"cmd":"pause"}` / `{"cmd":"resume"}` | user pause, independent of occlusion |
| `{"cmd":"quit"}` | tear down surfaces and exit |
| `{"cmd":"set","fps":60}` | global frame rate |
| `{"cmd":"set","sleep":"fullscreen\|maximized\|never"}` | sleep policy |
| `{"cmd":"set","scale":"physical\|logical"}` | render size policy |
| `{"cmd":"set","effects":"on\|off"}` | effect chain |
| `{"cmd":"set","fit":"cover\|stretch\|center\|contain"}` | canvas fit |
| `{"cmd":"set","color":"b,c,s,h,t,g"}` | picture colour adjust |
| `{"cmd":"set","time_scale":1.5,"volume":60}` | playback speed / audio volume |
| `{"cmd":"set-prop","props":{"<name>":"<value>"},"clear":false}` | scene user properties |
| `{"cmd":"mouse","x":0.5,"y":0.5}` | normalized pointer position |
| `{"cmd":"audio","on":true,"source":"monitor","sink":"default","bands":[16 floats]}` | spectrum control |
| `{"cmd":"dump","prefix":"…","count":N,"source":"scene"}` | write raw frames (diagnostics) |

A status reply looks like:

```json
{"ok":true,"kind":"scene","path":"…","playing":true,"suppressed":false,
 "user_paused":false,"frames":1234,"fps":60,"decode_w":2560,"decode_h":1440,
 "outputs":1,"cpu_s":1.17,"rss_mb":39.0}
```

`suppressed` means "the renderer is stopped because the output is covered", `user_paused`
means "the user asked for a pause". They are separate so the UI can show the right thing
and so resuming after a fullscreen window closes never overrides a user pause.

`scripts/we-ctl` is a thin client for exactly this protocol, useful for scripting and for
verifying a build without the GUI:

```sh
we-ctl status
we-ctl apply-scene ~/Templates/<id>      # a bare <id> works too: same library-root lookup
we-ctl set-prop MyColor="1 0 0"
we-ctl set fps=30
```

---

## 3. Renderer: from scene.json to draw calls

### 3.1 Parsing

`we-scene` is dependency-light and free of GL. Three layers:

* **`pkg`** — the PKGV container. The magic is not at offset 0 (there is a 4-byte length
  prefix), and each entry's `offset` is relative to the *end of the header*, not to the
  file. Both are easy to get wrong and both fail loudly if you do.
* **`tex`** — TEX containers: versioned header, image entries, embedded payloads (raw,
  DXT/BC, PNG/JPEG), plus the wrap-mode flag the renderer needs to pick `REPEAT` or
  `CLAMP`.
* **`scene`** — the `scene.json` object graph: camera, general (clear colour,
  orthogonal projection, bloom/hdr flags), objects (image / particle / text / puppet /
  sound), and the effect list attached to each object.

### 3.2 Scene compilation

The renderer turns the object graph into a flat list of **passes**:

```rust
struct Pass {
    target: Option<usize>,      // None = screen, Some(i) = render target i
    layer:  DrawLayer,          // shader + geometry + texture bindings
    units:  Vec<(u32, Src)>,    // texture units: source = texture | RT
    blend:  Blend,              // WE blend mode
    mvp:    [f32; 16],
}
```

Each object contributes its texture quad (or its MDLV mesh), then one pass per effect in
its chain. Effects that write to an intermediate RT get an RT allocation; the RT table is
reference-counted and buffers are aliased when their lifetimes do not overlap, which is
what keeps a 40-pass scene inside a sane VRAM budget.

Render target names (`_rt_<something>`) matter: a layer can reference another layer's
output by name (a "linked layer"), so compilation builds a dependency graph first and
orders passes accordingly.

### 3.3 Per-frame work

```
draw_frame():
  advance clocks (scene time, animation phases, particle simulation, audio bands)
  for each pass in order:
      bind target FBO (or the screen FBO)
      bind shader, upload uniforms (frame scalars, object matrices, effect combos)
      bind textures / RT attachments
      draw
  resolve to the output surface (GL blit, or hand the dmabuf to Wayland)
```

Frame cost is dominated by full-screen effect passes: a scene with a 12-effect chain is
12 full-screen draws per layer that owns the chain. `WE_PROFILE_PASSES` reports per-pass
GPU time so this is measurable rather than guessed; scissoring and RT downscaling are
available but off by default because they change pixels.

### 3.4 Subsystems inside the renderer

| Module | Responsibility |
|---|---|
| `gl_ctx` | EGL/GLES context, FBOs, textures, program cache, draw calls, dmabuf import/export |
| `shader` | GLSL assembly for WE-style effects (combos → `#define`s), uniform plumbing |
| `fuse` | Merges consecutive compatible effect passes into one draw |
| `mdl` | MDLV mesh parsing and MDLSv2 skeletal animation (bone matrices per frame) |
| `text` | Text layout inside the scene's layout box, glyph rasterisation, tinting |
| `propscript` | Runs the author's embedded JavaScript property scripts in `boa` |
| `particles` | Particle systems: emitters, forces, sprite sheets, collision modes |
| `parallax` | Parallax layer offsets from pointer position |
| `audio` | Spectrum smoothing / feeding `g_AudioSpectrum*` style uniforms |
| `video` | Video-backed layers (texture upload from the decode pipe) |
| `passtime` / `profcls` | Pass-level GPU timers and classification for profiling |
| `trace` | Opt-in tracing hooks (env-gated, zero cost when off) |

### 3.5 Determinism

The offline CLI pins the wall clock (`2025-01-01T00:00:00Z + t`), steps animation at a
fixed `1/30 s`, and can freeze time entirely (`--t`). Same scene + same flags ⇒ same
pixels, which is what makes "did my change alter the output?" a byte comparison instead of
an opinion. `--mouse` feeds the same pointer path the kernel uses, so mouse-driven effects
can be verified offline too.

---

## 4. Kernel internals

### 4.1 Surfaces

One `zwlr_layer_shell_v1` surface per output, `Layer::Background`, anchored to all four
edges, exclusive zone 0. With fractional scaling the surface is configured at the
output's physical pixel size and `wp_viewport.set_destination` is set to the logical
size, so the compositor does not scale our buffer — we render at native resolution.

Surfaces are created lazily: before the first `apply` the kernel holds no background
surface at all, so it cannot fight another wallpaper daemon for the layer.

### 4.2 Input

Pointer position reaches the renderer two ways:

1. `wl_pointer` on our surface — works when the pointer is literally over the wallpaper
   (i.e. over the desktop, not over a window).
2. `ext_image_copy_capture`'s cursor session — gives the global pointer position without
   capturing frames, so it works while the pointer is over any window.

The control plane offers a third source (`{"cmd":"mouse"}`) for scripting, which also
overrides the other two for a short window so a script's synthetic position is not
immediately overwritten.

### 4.3 Sleep and occlusion

The kernel tracks whether a fullscreen (or, optionally, maximized) toplevel covers each
output, using the compositor's toplevel list. When covered:

* the render loop stops scheduling frames,
* the ffmpeg child for video wallpapers gets `SIGSTOP` (so decoding truly stops, not just
  presentation),
* the control socket keeps answering — `status` must stay responsive even while sleeping.

On reveal, `SIGCONT` + one frame, and the scene clock resumes from where it was.

### 4.4 Failure handling

A wallpaper engine sits in the compositor's path; if it wedges, the desktop wedges. Three
guards exist for that, all in `we-native`:

| Guard | Protects against |
|---|---|
| `watchdog` | main loop stalls (a single lap taking far longer than a frame). Distinguishes a transient hiccup from a deadlock: transient stalls are tolerated up to a bound, repeated ones exit with a specific code so systemd restarts us. |
| `commit_guard` | a frame submit that never returns (driver/compositor not consuming buffers). Bounded pending commits + a deadline, then the surface is recreated rather than blocking forever. |
| `display_guard` | the Wayland connection or the EGL display dying (`EPIPE`, `EGL_CONTEXT_LOST`, would-block storms). Installs a panic hook, keeps the failure attributable, and exits in a way that the service manager can act on. |

`slowloop` is the attribution layer: it records which phase the loop was in, so a stall
report says "we were inside `eglSwapBuffers`" instead of "something took 8 seconds".

Fault injection flags (`--debug-stall-ms`, `--debug-stall-count`,
`--debug-kill-wayland-ms`, `--debug-drop-surface-ms`) exist so these paths can be
exercised on purpose. They are inert unless passed.

### 4.5 App state restore

The kernel is deliberately stateless: the UI decides, the kernel executes. The one
exception is a single "last applied" record so a restarted service does not leave the
desktop empty. Per-wallpaper property overrides and per-scene frame rates live in the UI's
config and are re-sent when the UI reconnects — the kernel does not remember preferences.
That record stores a path, so it goes through the same id fallback as `apply` (4.6): a
wallpaper that was moved to another library root is found again instead of the desktop
coming up empty.

### 4.6 Library roots (where the wallpapers come from)

Both the UI and the kernel resolve wallpapers through the same list of **library roots**
(`crates/wallpaper-hub/src/wp/library.rs`, mirrored in `crates/we-native/src/library.rs`
because the kernel cannot depend on cosmic-config):

| Source | Default |
|---|---|
| local library roots | `~/Templates` |
| Steam workshop content (**optional**) | `~/.local/share/Steam/steamapps/workshop/content/431960`, plus `.steam/steam` and the Flatpak path, plus every `"path"` in `libraryfolders.vdf` — only those that exist |

A root contains one directory per wallpaper (`<root>/<id>/` with `project.json` +
`scene.pkg`, or `<root>/<any name>/` — the display name comes from `project.json`). A root
that is itself a project also counts as one wallpaper. Roots are resolved as:
`WPHUB_LIBRARY_DIRS` (env, colon-separated) → the UI's `library_dirs` config list →
`~/.local/share/wallpaper-hub/library_dirs` → `~/Templates`. The UI writes that state file
(UI roots *and* the optional Steam roots) and the kernel re-reads it on every `apply`, so
changing the roots never needs a kernel restart.

`apply` therefore accepts three things: an absolute directory, a file, or a bare
**wallpaper id** (= directory name). If the path does not exist the kernel retries by id
across the roots — which is what keeps a wallpaper working after it is moved out of the
Steam tree. The same fallback is applied to `~/.cache/wallpaper-hub/last-applied`.

---

## 5. Data on disk

| Path | Owner | Contents |
|---|---|---|
| `~/.config/cosmic/…/wallpaper-hub/v1/` | UI | settings, per-scene fps table, per-wallpaper colour and property overrides |
| `~/.cache/wallpaper-hub/` | UI | workshop metadata cache, promo image cache, thumbnails |
| `~/.cache/wallpaper-hub/last-applied` | kernel | the single "restore this after restart" record |
| `~/.local/share/wallpaper-hub/library_dirs` | UI writes, kernel reads | the effective library roots, one per line |
| `$XDG_RUNTIME_DIR/we-native.sock` | kernel | control socket |

Nothing is written into a Steam library, and no Steam account data is touched. Library
directories are opened read-only.

---

## 6. Threading

* **UI**: one iced event loop; all socket I/O happens on a background task, results arrive
  as messages.
* **Kernel**: one calloop loop owns the Wayland connection, the surfaces and the GL
  context. The control socket lives on its own thread and posts commands into the loop
  through an eventfd, so a slow client can never stall a frame.
* **Renderer**: called from the kernel loop thread (single-threaded GL). No GL object is
  touched from any other thread.
* **ffmpeg**: a child process per video wallpaper, `SIGSTOP`/`SIGCONT` managed by the
  kernel.

---

## 7. Where the format knowledge comes from

`scene.pkg` and `scene.json` are undocumented. The parsers here were derived from the
data itself, and cross-checked line by line against public reimplementations
(`open-wallpaper-engine`, `linux-wallpaperengine`, `wallpaper-scene-renderer`). Code
comments cite those references by file and line where a decision depends on them — those
paths refer to the upstream projects, which are **not** vendored in this repository.

Where the original engine's behaviour could not be established, the code says so and puts
the assumption behind a named constant or an env switch, so it can be A/B tested against a
reference frame instead of quietly becoming "the way it works".
