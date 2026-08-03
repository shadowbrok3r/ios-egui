# Changelog

## v0.6.0 — 2026-08-03

Fixes the 401 that took the app offline. comfy-gate expires a signed-in session after 14 days and nothing renewed it, so the app went on presenting a dead token forever with no way to tell why — the websocket said "polling still works", which was a hardcoded phrase rather than a fact. A refused session is now cleared and named ("Session expired — sign in again, or set an API key"), a rejected credential ends the websocket's reconnect loop instead of retrying it every 30 seconds, and a credential carrying a character an HTTP header cannot hold is reported rather than silently dropped.

The glass is finished. Panes are lit by three soft pools of colour, because a backdrop blur can only reveal what is behind it and every surface stood on pure black — frosted panes read as flat plastic without them. Graph nodes are glass too. Edges are now a dim white hairline everywhere: a coloured outline is what stops a surface reading as glass, since real glass has no colour at its edge.

Graph editor: choosing a checkpoint or diffusion model seeds that model's recommended steps, CFG, sampler, scheduler, size and CLIP skip, the same way the Create tab already did — plus **Edit → Apply model defaults** for a graph loaded from a file, and **Edit → Apply recommended tags** to add the model family's quality tags to the prompt nodes. A model dropdown's filter box no longer closes the dropdown the moment you tap it, and the list no longer hides everything past its first 200 entries.

## v0.5.1 — 2026-08-03

Fixes on top of 0.5.0: the app self-update now actually raises Android's install dialog (its PendingIntent was rejected outright on Android 14+) and reports success or failure; the device photo/video picker no longer leaks a bitmap per tile.

## v0.5.0 — 2026-08-03

Frosted glass: real backdrop blur behind every window, menu and sheet (the app now runs on the glow renderer). Gallery metadata panel no longer leaks swipes to the image behind it, and Back closes it instead of the viewer. The View menu fits the screen instead of being clipped on the right, and its album/model/LoRA lists use the full height. The queue and Trash sheets no longer let a scroll behind them refresh the gallery.
