# Changelog

## v0.14.1 — 2026-08-25

**Fixed: images generated on the device went nowhere.** A Local NPU / Local Anima render finished, logged its size, and then existed only as the texture in that one frame — the Create pane deliberately stays quiet on a finished result because results belong in the Gallery, but nothing had ever put an on-device render *into* the Gallery, and the server that lists it never saw it.

On-device renders now file themselves. Each one is written into the phone's image store under an **on-device** folder and spliced into the gallery listing newest-first among the server's rows, so it shows up in the grid, lights the Gallery nav badge, and opens in the viewer like anything else — with no server involved at all, since thumbnails, the full-screen image and delete all read the local file. They survive both cache eviction and Settings → Clear cache: unlike a cached server image there is no copy to re-download, so nothing is allowed to reclaim them. Renders from earlier sessions are picked up too.

## v0.14.0 — 2026-08-14

**Every look axis now has a color**, used everywhere the axis appears: the combobox labels, the extract sheet's headers and tag chips, the Look book and builder section titles, and — most usefully — the prompt's chip view, where a tag that belongs to an axis wears that axis' hue (appearance pink, expression amber, outfit azure, pose green, camera orange, environment aqua). One glance at the prompt now shows which pieces are swappable and where they'd go.

**Per-axis extract.** Each axis dropdown's entry is now "Extract appearance" / "Extract outfit" / etc., pulling only that axis' tags into the sheet; the standing Extract from prompt button still does all axes at once.

**Fixed: the Sweep window's axis dropdown was unusable** — its popup opened underneath the window (these centered windows sit on a layer above the one dropdown popups render on), so nothing in it could be tapped. The axis choice is now a flat color-coded row of buttons, and the Manage editor's new album picker — which had the same latent bug — became an inline expandable list.

## v0.13.0 — 2026-08-13

**Build a prompt** — the from-scratch flow, next to Look book. Step one picks the model *visually*: a tile grid where every checkpoint shows your newest render made with it (models without renders wait in a collapsed list), and picking one brings its recommended settings and its remembered look along. Step two sets the subject (tap-toggle 1girl/1boy/2girls/solo, plus free tags), then one Look-book step per axis — appearance, expression, outfit, pose, camera, scene — and a review step that shows the assembled prompt and can queue a generation on the spot. Every choice applies live, so closing the builder at any step keeps what you've built.

**Example albums for presets.** A preset can now point at a gallery album that showcases it: assign one in Manage (each preset row grew an "Example album" picker), then long-press that preset's tile — in the Look book or the builder — to jump straight into the album and see many examples instead of one photo.

## v0.12.0 — 2026-08-13

**Look book** — a visual preset browser next to Extract. Every preset is a tile: its photo when it has one, name underneath, a pink ring on the active pick, grouped per axis; tap to apply, tap the active tile to clear, and the window stays open so you can dress a whole prompt visually. Presets created by Extract from a gallery image photograph themselves automatically with that image; anything else gets a picture from the gallery viewer's new **Set as preset photo** menu (character looks already had theirs). Photographed presets sort to the front.

The look comboboxes reorganize around your own material: **Global presets now sit first and open**, while each character and the long built-in list fold into collapsible sections — the section holding the current pick starts open, so nothing lives at the bottom of a deep scroll. The extract sheet also **recognizes tag sets you already saved**: an axis whose toggled tags exactly match an existing preset says so and swaps that preset in instead of minting a duplicate.

Also: the Sweep window's checkpoint and preset lists truncate long names instead of pushing the window past the right edge of the screen.

## v0.11.0 — 2026-08-13

**Sweep** (Create tab, under the Batch/Variants steppers): pick two or more checkpoints — or two or more presets on one look axis — and queue the same prompt at the same fixed seed across all of them, each job running at its model's recommended settings. Results collect into a numbered "Sweep N" album as they land, so comparing models side by side becomes one tap and a gallery visit instead of an evening of manual switching. Your own model, seed and look picks are restored the moment the jobs are queued.

**Expression** joins the look axes: a new combobox with 22 built-in presets (Smile through Ahegao), full extract support, and tap-to-swap between expressions on a prompt chip. The gallery corpus run showed expressions were the most-used tags that had no axis to live on.

The prompt lint now catches typos: a tag unknown to the tag dictionary, the axis classifier, and your LoRA triggers — but within an edit or two of a real tag — gets a "did you mean" warning whose fix replaces it in place. It would have caught the `tonails` (54 images) and `skindention` (17) lurking in a long-standing template prompt. LoRA trigger words, quality tags, and video prose are exempt, so nothing legitimate gets nagged.

**The look follows the model.** Picking a checkpoint in the Models pane now restores the axis picks you last used with it — appearance, expression, outfit, everything — captured automatically every time you change a pick. Switch from your Illustrious setup to the Anima one and each comes back wearing its own look; models you've never dressed keep whatever is currently applied. Programmatic switches (sweeps, character cards, remixes) deliberately don't touch this memory.

**Presets from an image** (NPU builds): the WD14 tag sheet grew an **Extract presets** button — the tagger's read of any gallery image flows through the same classifier into the extract sheet, so a render you like becomes appearance/expression/outfit/scene presets (or additions to a character) without retyping a single tag. Saving from an image only creates presets; your prompt is untouched.

## v0.10.0 — 2026-08-13

Extraction now covers every look axis, not just appearance. **Extract from prompt** (a button atop the Appearance/Outfit/Pose/Camera/Environment section, and an entry in each of those dropdowns) splits your whole prompt in one pass: detected tags appear as tap-to-toggle chips grouped per axis, each group with its own editable preset name, and one save turns them all into swappable combobox picks — the effective prompt unchanged. Outfit catches accessories by their last word, so `black choker`, `hair ornament` and `toe ring` classify without an exhaustive list; poses, camera framing and scenery tags work the same way.

The appearance classifier also learned from the first field test: compound hair tags (`long straight hair`), the full toenail color range (`aqua toenails`), skin finishes (`soft skin`, `wet skin`), and non-human features by last word (`cat ears`, `fox tail`, `demon horns`). Every tag inside the ~140 built-in look presets now classifies to its own axis under test, so the extractor can round-trip anything the preset system can inject.

The vocabulary was then tuned against the real gallery on comfy-gate — every prompt from 2,241 generated images. The high-frequency tags that slipped through now classify (body descriptors, toe/knee poses, `point of view` / `full body view` framing, mood lighting), sentence-style video prose is excluded outright (tags longer than five words never extract, so a Wan prompt can't be shredded), and what deliberately stays in the prompt is exactly the non-look material: quality tags, character names, LoRA trigger words, and expressions.

## v0.9.0 — 2026-08-13

Character appearance is now hot-swappable. **Appearance** joins the Create-tab look comboboxes: a named bundle of the tags that describe who you're drawing — hair color and style, eyes, nails, toenails, skin, body — applied and removed as one unit, so trying the same subject across checkpoints stops meaning hand-editing hair and toenail color every time. **Extract from prompt**, inside that dropdown, reads those tags out of the prompt you already wrote, shows them grouped with a checkbox each, and moves the checked ones into a named preset — the effective prompt is unchanged, but the appearance is now a dropdown pick. Presets save globally or under the active character, and swapping to another (or to None) removes exactly what the old one injected, weights included.

For one-off fiddling, tap a recognized appearance chip in the prompt's chip view: a new Swap strip offers the tag's whole family — every hair color, the breast-size ladder, nail and toenail polish colors, skin tones — and replaces it in place, keeping any `(tag:1.2)` weight wrapper.

## v0.8.0 — 2026-08-11

Graph editor: nodes copy between workflow tabs. Long-press a node → **Copy** (or **Copy with inputs**, which brings the whole upstream chain), then long-press the canvas in any tab → **Paste nodes**. The pack rides the system clipboard, so it survives closing tabs and can travel between phones; classes the target server lacks are reported by name instead of silently dropped.

Apps now insert as a single collapsed node — ComfyUI's subgraph, phone edition — placed exactly where your finger opened the menu, wearing a violet coat so it can be spotted across a big graph. Its sockets are the app's real inputs (image, model, …), its widgets are the knobs, and it unfolds into the true nodes only at queue time. Long-press → **Unpack app** unfolds it in place for hand-editing. They also appear in Add node under "apps". **Save tab as app** now states the app's seams instead of implying them: an Inputs row lists what it will consume from the surrounding graph, and when more than one IMAGE end dangles, an Output picker asks which one is the result rather than guessing the last.

Enhance steps can now run on the **source image**: every image step has a "Runs on" choice between the finished render and the img2img input, the latter splicing it between load and encode — so an oversized source can be shrunk *before* it costs (or freezes) a full-resolution sampler pass, instead of being upscaled again after one. A step aimed at a source that doesn't exist says so in the run note rather than silently doing nothing. New builtin **Resize to megapixels** scales to a total pixel budget whatever size came in — point it at the source to tame any huge input.

The graph editor's image picker closes itself once you tap a picture (the choice shows on the node; keeping the window up just cost an extra tap) and grows a full-width Close button — the title-bar ✕ was a fingertip-hostile target.

## v0.7.0 — 2026-08-05

Built on egui 0.36. Two of its changes needed answering before they became bugs you would have hit.

egui now decides a press is a drag the moment your finger leaves the widget, rather than after the 16 points of travel this app tunes for finger wobble. Left alone, a sloppy tap on a prompt tag would reorder the prompt, the Queue and Create buttons would creep across the screen a few points at a time, tapping a pin in the graph editor would flick out a wire and sometimes drop a connection on release, and a tap near the minimap's edge could throw the view to a far corner of the graph. All four now wait for real travel again. Panels also grew an invisible drag strip along their edge while collapsed — on a phone that sits exactly where your thumb rests, swallowing taps and painting a bright line over the bottom of the screen — so it is turned off.

Password fields now tell the keyboard they are passwords. Until now every field looked the same to the hidden text bridge, so API keys and tokens went into the keyboard's suggestion and personalized-learning stores like ordinary words. Deleting around the cursor is also handled by egui directly instead of being replayed as a run of backspaces around a lifted composition, which is what made a mid-word backspace occasionally eat the wrong character or leave the caret behind.

## v0.6.0 — 2026-08-03

Fixes the 401 that took the app offline. comfy-gate expires a signed-in session after 14 days and nothing renewed it, so the app went on presenting a dead token forever with no way to tell why — the websocket said "polling still works", which was a hardcoded phrase rather than a fact. A refused session is now cleared and named ("Session expired — sign in again, or set an API key"), a rejected credential ends the websocket's reconnect loop instead of retrying it every 30 seconds, and a credential carrying a character an HTTP header cannot hold is reported rather than silently dropped.

The glass is finished. Panes are lit by three soft pools of colour, because a backdrop blur can only reveal what is behind it and every surface stood on pure black — frosted panes read as flat plastic without them. Graph nodes are glass too. Edges are now a dim white hairline everywhere: a coloured outline is what stops a surface reading as glass, since real glass has no colour at its edge.

Graph editor: choosing a checkpoint or diffusion model seeds that model's recommended steps, CFG, sampler, scheduler, size and CLIP skip, the same way the Create tab already did — plus **Edit → Apply model defaults** for a graph loaded from a file, and **Edit → Apply recommended tags** to add the model family's quality tags to the prompt nodes. A model dropdown's filter box no longer closes the dropdown the moment you tap it, and the list no longer hides everything past its first 200 entries.

## v0.5.1 — 2026-08-03

Fixes on top of 0.5.0: the app self-update now actually raises Android's install dialog (its PendingIntent was rejected outright on Android 14+) and reports success or failure; the device photo/video picker no longer leaks a bitmap per tile.

## v0.5.0 — 2026-08-03

Frosted glass: real backdrop blur behind every window, menu and sheet (the app now runs on the glow renderer). Gallery metadata panel no longer leaks swipes to the image behind it, and Back closes it instead of the viewer. The View menu fits the screen instead of being clipped on the right, and its album/model/LoRA lists use the full height. The queue and Trash sheets no longer let a scroll behind them refresh the gallery.
