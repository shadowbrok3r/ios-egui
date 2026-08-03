# Changelog

## v0.5.1 — 2026-08-03

Fixes on top of 0.5.0: the app self-update now actually raises Android's install dialog (its PendingIntent was rejected outright on Android 14+) and reports success or failure; the device photo/video picker no longer leaks a bitmap per tile.

## v0.5.0 — 2026-08-03

Frosted glass: real backdrop blur behind every window, menu and sheet (the app now runs on the glow renderer). Gallery metadata panel no longer leaks swipes to the image behind it, and Back closes it instead of the viewer. The View menu fits the screen instead of being clipped on the right, and its album/model/LoRA lists use the full height. The queue and Trash sheets no longer let a scroll behind them refresh the gallery.
