# Changelog

## 0.4.0

- **Sync with the desktop.** Files gained a host and token; Pull takes whatever the desktop app is
  showing, Push replaces it. Over Tailscale that works from anywhere, not just your own network —
  the desktop binds its `100.x` tailnet address specifically, never `0.0.0.0`, and refuses to serve
  off-loopback without a token. An open port here would let anyone who can reach it rewrite the
  design you are looking at.

## 0.3.0

- **Photograph a surface and cast it.** Pick a photo from the gallery and it becomes an alpha on
  the band. Shoot flat, even light — a photo records the lighting as much as the surface, and
  raking light turns into bumps that are not there, which the picker now says on screen. Imports
  are kept on disk, so they survive a restart instead of coming back as a blank layer.
- **1:1.** The ring renders at true physical size, from the panel's real `DisplayMetrics.xdpi`
  rather than the density bucket Android rounds to — hold the phone against a finger or a mandrel
  and compare. The toggle only appears when the panel reports a DPI worth trusting.

## 0.2.0

The alpha library, and somewhere to put your work.

- **Alphas** — all 16 procedural patterns as a thumbnail grid. Tap one to put it on the band; the
  layer snaps to the side faces where the profile has any, because that is where relief casts
  clean. Repeats and depth adjust live, and the whole set regenerates at 128, 256 or 512.
- **Files** — name, save and reopen designs; export an STL at 1024x320 straight to the share sheet
  with a real `model/stl` type; copy or paste a whole design as JSON, which needs no network and
  works when nothing else does.
- `Alpha::from_bytes` in the core, so an image that arrives as bytes rather than a path can become
  an alpha — the shape every Android file handoff takes.

## 0.1.0

First release. RingDesigner on the phone, for designs that have to come out of a two-part sand
mould.

- **Ring** — the band in 3D, ported to OpenGL ES. One finger orbits, two pinch and pan. Metal,
  draft and normal shading, a barycentric wireframe, and the castability verdict with its undercut
  percentage.
- **Band** — the unrolled `(u, v)` surface with the composited height field drawn underneath, so
  you can see where the metal actually lands. Castability zones are tinted by what they can hold,
  with the ring-angle ruler and the seam marked.
- **Pen** — pressure is millimetres of metal, not opacity, and the ceiling comes from the local
  draft angle: about 1.6 mm on a squared side face, about 0.05 mm on the crest of a half-round.
  Press past what the surface allows and the stroke says so. Hover reads out the limit before you
  commit. The barrel button and the flipped tip carve; palm and finger contacts are rejected while
  Pen only is on.
- **Tile** — draw one motif that wraps in both axes and marches round the ring an integer number of
  times, so it meets itself at the joint by construction.
- **Bench** — per-stage timings of the geometry core on this device.

Drawings are stored as strokes inside the design, not as baked images, so a `.ring.json` stays
self-contained, re-rasterizes at any resolution, and opens on the desktop unchanged.
