# Changelog

## 0.10.0

- **On-device models, behind a flag.** Built with `--features local-npu` and a
  model pack on the phone, two things become possible. **Describe a pattern** and
  the NPU makes a tileable one — the latent is rolled between denoising steps so
  the model never sees the wrap as an edge — and the app then measures it against
  the sand's detail floor before offering it, which is the part nothing else
  does. And **index the library** to ask "what else is like this" by long-pressing
  a tile, with near-duplicates flagged rather than quietly accumulating.

  A default build links none of it, and Files says plainly which of the three
  things is missing when it is: the feature, the runtime, or a pack.
- **The pen cuts metal.** Tilt and azimuth now shape the stamp: a pen held
  upright cuts a round bead, a pen laid over cuts a long flat facet along the
  way it leans — because a graver's cut section is set by how the tool is held.
  Pressure still means millimetres of depth; tilt shapes the tool, it does not
  change how deep it goes. Designs written before this open unchanged, and a
  phone-drawn tilted stroke opens on the desktop with the same footprint.
- **Every sample, and only the pen's.** The S-Pen reports 120–240 times a second
  against a 60 fps screen, and the app was keeping one sample a frame, so a fast
  arc landed as a polygon. It now takes every one. And the depth of a stroke came
  from the last pressure seen anywhere — so a hand resting on the glass was
  changing how deep the pen cut. Each sample now carries its own.
- **Hover is a pre-flight.** Before the pen touches, the brush circle shows two
  rings: the depth this spot will take and the part it will refuse. Cross from a
  side face onto the crest and the inner ring collapses. A brush finer than the
  sand can hold is ringed amber. The preview fades in as the tip approaches.
- **The barrel button stopped being a second eraser.** The flipped tip still
  erases, and so does Carve — the button now pans and zooms while held, so the
  band can be moved without putting the pen down, taps to pick up the depth under
  the tip as the new brush depth, and on the second button, steps undo.
- **Haptics that mean something.** A tick the moment the ceiling starts refusing
  the depth being asked for, a lighter one crossing between castability zones, a
  buzz when a build makes the verdict worse, and detents on the quarter sizes.
- **Draw the face, draw the section.** Sketch a closed plan with the pen and it
  becomes the signet head's outline — a real `CustomOutline` carried in the
  design, so it opens on the desktop unchanged and is saved to the outline
  library the desktop reads. Sketch half a cross-section and it becomes a Custom
  profile. Monotone by default, which *is* the no-undercut guarantee; there is an
  explicit switch to draw an undercut and the line turns red while it is on.
- **Pick the stone.** Fourteen cuts, faceted or cabochon, the sizes each cut is
  actually sold in, and three setting styles — with the carat weight and depth
  read back as you choose. Auto pavé and Channel set use what you picked instead
  of the 1.5 mm round they were hardcoded to, and you set the arc: centre, span
  and whether the rows stagger. A fill can also come off the side face onto a
  strip of the band, which is the honest answer to "no side face to fill" on a
  domed profile — and it says what it costs.
- **Add a layer.** Border, milgrain, a gem seat pad, an eternity row, three
  curve presets and a halo. A curve lands on the wider side face when the
  profile has one, retargeted and gated there, because a rail across the crown
  leans back on its crest-side flank while the same wire on a face square to the
  pull measures 0.000%.
- **A Report sheet.** Dimensions, weight in every alloy in grams *and*
  pennyweight, and a per-seat stone table — footing, seat diameter, edge
  clearance, pavilion room, bridges, every bench warning, and the tightest
  neighbours with the gap that decides. All of it was already computed on every
  build and thrown into a tooltip.
- **Files that behave.** Newest first instead of alphabetical, recents at the
  top, rename, and delete behind one confirmation — app storage is unreachable
  from any file manager, so a file the app cannot delete is permanent until
  uninstall. Saving over an existing name warns once instead of silently
  replacing it: two designs called "untitled" used to be one file.
- **It remembers.** The desktop's address and token, the brush, the shading, the
  stone, the shrink alloy — all kept between launches. The sync fields were
  documented as remembered and never were.
- **Renders file under RingDesigner.** They were going into another app's
  gallery album; the folder now comes from the app's own name.
- **The app hears the OS.** `onPause` reaches the app, so an edit made a moment
  before backgrounding is flushed rather than lost to a reaped process.
- **The stack, on the phone.** A Layers sheet lists every layer with its kind,
  and a tap opens it: rename, opacity, blend, and the window that decides where
  round the ring and where across the band it acts — including **snap to side
  faces**, which is how ornament gets onto the two faces square to the mould
  pull, the ground that measures 0.000% undercut at any relief the band holds.
  Mute, solo, copy, delete, and move up, down, to top or to bottom. A layer the
  sand cannot hold is amber, with the finding underneath it.

  Until now the phone could make layers and the only thing it could do to them
  was Clear layers, which deleted all of them — so Auto pavé was one-way, and a
  design pulled from the desktop arrived with a stack it could not open.

  No drag-to-reorder, deliberately: a drag source inside a scrolling touch list
  steals the scroll.
- **Undo, at last.** Every edit on the phone is undoable now, not just the last
  paint stroke: a size change, a shank change, an Auto pavé, a cleared stack, a
  loaded template, a pasted design. Undo and Redo sit in the Ring toolbar and
  name the step they would take back — "Half Round", "Added a layer" — because
  the name is read out of the difference between two designs rather than typed
  at the call site. Long-press Undo for the whole timeline and tap to jump.
  Clear layers used to be one tap with no confirmation and an autosave 90 ms
  behind it; it is now one tap and one tap back.
- **The findings open.** The DFM chip is a button now: tap it and every finding
  reads as text — which layer, which texture, what it measures, and the floor it
  misses. A tiling gets **Fit to the floor**, which sets the repeats to the most
  that pattern can carry and still cast. When no repeat count clears it, the
  layer is left alone and the sheet says how tall the cell has to be instead,
  which is the number worth designing the band around.
- **The sand answers before you commit.** Every tile in Alphas carries its
  finest stroke or gap in millimetres, measured on the cell the current repeat
  count would actually lay down. Anything under the detail floor is rimmed
  amber. The refusal used to arrive after the pattern was on the band.
- **Sand or lost wax, on the phone.** The Design sheet has the process picker
  the desktop has had, with the Delft clay and Petrobond presets under it and
  the three floors printed underneath. Returning to sand restores a sand floor —
  the core writes floors only on the way into lost wax, so coming back would
  otherwise leave a Delft-clay ring judged at investment numbers and reading
  Castable on walls it will not fill.
- The verdict chip names the process. Under lost wax the undercut percentage is
  measured and reported but never gates, so "Castable · 3.10%" is true and
  unreadable without it.

### Logged late

- **Stone map** in Share — every stone to scale in plan and on the unrolled
  band, with the census's tight gaps drawn in red and the gap that decides
  written on the line. Shipped after the 0.9.0 entry was written.
- **The DFM reads the textures.** Findings stopped trusting a tile's cell pitch
  and started measuring the mask itself by granulometry, so a fine-lined alpha
  on coarse cells is caught.

## 0.9.0

- **Graph tab.** The design's recipe graph, edited on the phone: convert the
  current design to nodes, or start from the simple or a template graph; drag
  to pan, pinch to zoom, long-press for the node and background menus, drag
  pins to wire; Arrange, Fit, Lock and Bake. A graph arriving with a pulled
  design shows up the same way. The build worker evaluates the graph before
  every build, and the editing tabs say "driven by the graph" with a Bake
  button while one is in charge.
- **Exports off the UI thread.** STL, 3MF, GLB, the casting sheet, the render
  and the turntable each build on their own thread and open the share sheet
  when the file lands; several can run at once and none is dropped behind a
  preview build.
- **Save a copy to Downloads.** The design file into shared storage through
  MediaStore, where it survives uninstalling the app.

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
