# Changelog

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
