# Changelog

## 0.8.2

- Cut dome slider on the signet head: the face cut from a swollen dome
  instead of the prism construction — no pinched corners, no prism walls.
- Diamond and Cross face outlines.

## 0.8.1

- **Cope and drag.** A fifth shade mode splits the ring into its two mould halves — cope in blue,
  drag in sand — with the parting line glowing bright between them, so where the sand splits is
  visible at a glance.

## 0.8.0

- **The galactic glass look.** The app now wears the same theme as the rest of the family: an
  AMOLED-black page lit by soft colour pools, violet-glass surfaces with real backdrop blur
  behind the bottom chrome, hot pink for what's chosen and aqua for what's ready.
- **A bottom bar that fits.** Ring, Band and Tile stay labelled, the rarer tabs become icon
  squares, and nothing runs off the edge of the screen any more.
- **Design slides up over the ring.** The design controls are a collapsible sheet now — tap
  Design and the sliders rise over the live 3D view, so every change shows on the mesh as you
  drag instead of after a tab-switch. Tap again to tuck it away.
- **Menus instead of button walls.** Files now offers File / New / Export / Share menus that
  open upward (never under the gesture bar), with the templates tucked into New.

## 0.7.0

- **Keyframed shanks.** The new Keyframes shape in Design hands you the band itself: set width,
  thickness and crown at your own stations round the ring and they blend smoothly, closing on
  themselves. Taper, pinch, bombé — or something nobody named yet.
- **The engine underneath grew too.** Crisp mm-true pattern edges, openwork carving with a floor
  over the finger hole, pattern rows that flow along a drawn wave, and export meshes that spend a
  third fewer triangles for the same accuracy — designs made on the desktop with these open here
  and preview correctly.

## 0.6.0

- **A real design editor.** The new Design tab edits the whole ring on the phone: US size,
  profile style and dimensions, every shank shape — including Wave, Twist, Split and Signet with
  its head controls — a second head for a toi et moi, and one-tap stock generators for pavé and
  channel setting. Templates in Files start a design worth editing.
- **Wall heatmap.** A fourth shade mode colours the ring by metal under the surface: red where
  the sand will not fill, through amber and green to blue-grey where it is heavy, with the legend
  in the toolbar. Same ramp as the desktop.
- **Touch the ring, get answers.** Long-press the 3D view and a chip reads back where you landed
  (angle and position across the band), the relief height, the wall thickness, the draft class,
  and which layer put metal there.
- **Stones on their seats.** The viewer now shows faceted preview stones sitting on seat pads and
  eternity rows — display only, never in the mesh, exactly as the desktop draws them. The stones
  chip counts them and carries every bench warning; a DFM chip flags detail the sand cannot hold.
- **Share it moving.** Three new shares: a polished PNG render, a looping turntable GIF, and a
  GLB — glTF in real metres, which AR viewers and web viewers open as a ring-sized ring.

## 0.5.0

- **The honest verdict.** Castability now comes off the surface itself rather than the preview
  mesh, so facet noise can never fake an undercut: the chip shows the field verdict, the undercut
  share, and the thinnest wall over the finger hole — and when something does undercut, the note
  names the arc and the layer to blame ("caused by \"Flat boss\"; muting it clears it").
- **As-cast.** A toggle softens the preview at the sand's own detail radius, so beads merge and
  fine cells mush on screen the way they will in the pour. Display only — exports stay exact.
- **3MF export and pattern shrink.** Share as 3MF (the file states its units, so nothing downstream
  guesses mm vs inches), and cut either export oversize for a chosen alloy's shrink — the file is
  renamed as a pattern so it cannot be poured as nominal by mistake.
- **One brush, both devices.** The pressure-to-millimetres math and the band-layer convention now
  live in the shared core, and the desktop's unrolled editor paints with them too: a band painted
  here opens on the desktop as the same layer, and vice versa.

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
