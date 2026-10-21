# Contributing

Thanks for taking a look. This project is a from-scratch engine, so the most valuable
contributions are precise bug reports about specific scenes, and small, well-explained
patches to the parser or renderer.

## Ground rules

1. **Never attach wallpaper assets.** No `scene.pkg`, `.tex`, extracted shaders, video
   files, or screenshots of someone else's wallpaper in issues, PRs or test data. Describe
   the scene by workshop id and by what is visually wrong. If you need to show a
   difference, crop to a region that only contains the artefact you are reporting and say
   which wallpaper it came from.
2. **No Steam credentials, no account automation.** The project does not log in to Steam,
   subscribe to items, or download anything. PRs adding that will be declined.
3. **Keep the crate boundaries.** `we-scene` stays free of GL and Wayland. `we-scene-render`
   never opens a Wayland surface. `we-native` never parses `scene.pkg` itself. That split is
   what lets the offline renderer exist.

## Building and testing

See [`docs/BUILDING.md`](docs/BUILDING.md). Before sending a patch:

```sh
# each crate has its own manifest; run these per crate
cargo fmt --manifest-path crates/we-scene/Cargo.toml
cargo clippy --manifest-path crates/we-native/Cargo.toml --all-targets
cargo test --manifest-path crates/we-native/Cargo.toml
```

If your change affects rendering, include an offline render before/after:

```sh
we-scene-render <scene dir> before.png --size 1280x720 --t 10
# apply your change
we-scene-render <scene dir> after.png  --size 1280x720 --t 10
```

and state whether the difference is intentional. `--t` and `--frames` make this
reproducible; without them you are comparing two different moments in an animation.

## Code conventions

* **Comments explain *why*, not *what*.** A comment that restates the line below it is
  noise. A comment that says "clamp here because the effect chain can hand us a negative
  radius, and the driver then drops the draw silently" is the point.
* **Cite your source when you had to reverse-engineer something.** If a constant comes from
  a reference implementation, name the file and line. If it came from measurement, say what
  was measured. If it is a guess, write "assumption" and put it behind a named constant or
  an env switch so it can be A/B tested.
* **Prefer a switch over a silent behaviour change** when you are aligning semantics with
  the original engine: default the new behaviour on only when you can show it fixes more
  than it breaks, and keep the old path reachable (the codebase uses `WE_*=0` for this).
* **No machine-specific paths.** Use `$HOME`/`$XDG_*` lookups or relative paths. Tests that
  need external data read a path from an environment variable and skip when it is unset.
* **Do not add debug switches that only make sense for one investigation.** If a knob is
  useful to users, document it in the README; if it is only useful for maintainers, put it
  in `docs/BUILDING.md` under diagnostics.

## Reporting a rendering bug

Please include:

* the wallpaper's workshop id (not the files),
* what you expected vs what you see, and where on screen,
* your GPU, driver version and compositor,
* the offline render command you tried, if any,
* the relevant env settings (`WE_EFFECTS`, `WE_RENDER_SCALE`, …).

A useful report narrows it to a subsystem: "static layers correct, particle layer shifted
up by ~4px at 1440p" is actionable; "scene looks wrong" is not.

## Commit messages

`area: what changed` — e.g. `renderer: fix mask sampling space for linked layers`.
Body: why it was wrong, and how you verified it. One logical change per commit.

## License

By contributing you agree that your contribution is licensed under the MIT license
(see [`LICENSE`](LICENSE)), and that you have the right to submit it — in particular, that
it contains no code or assets copied from Wallpaper Engine or from a project whose license
is incompatible with MIT.
