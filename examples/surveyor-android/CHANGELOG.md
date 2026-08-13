# Changelog

## v0.1.8 — 2026-08-13

New micro-motion instrument on the Live tab: a scrolling 60 GHz strip-chart
from the A121 presence bridge — breathing-band trace in aqua, fast motion in
pink, presence tinting the background, with distance and score readouts. Only
appears once the channel speaks, so setups without an XM125 see no dead chrome.

## v0.1.7 — 2026-08-06

Shortens the radar afterglow from 8 seconds to under 3. The live dot was
always current, but an 8-second streak trailing behind a walker read as the
map lagging. Measured for comparison: the server-to-app path adds 0.6 ms.

## v0.1.6 — 2026-08-05

egui 0.36: collapsed bottom bars no longer leave an invisible drag strip at the screen edge, and password fields now tell the keyboard not to learn them

## 0.1.5 — 2026-08-04

Finishes the galactic recolor: the frost tint behind the nav bar is violet
smoke (it was still green), and the launcher icon now wears the same palette —
pink sweep, aqua emitter, violet blob.

## 0.1.4 — 2026-08-04

Galactic neon recolor: hot pink sweep and live signals, aqua radar returns
and hover, electric violet CSI and info text, violet-cast glass on deep-space
black.

## 0.1.3 — 2026-08-04

Visual overhaul: a radar-phosphor glass theme (vibrant green/amber/cyan on a
lit near-black page, real backdrop blur behind the chrome), navigation moved to
a bottom bar that collapses while typing, the Record tab rebuilt so no button
is clipped, and the Live tab is now a proper sector-scan radar instrument —
range rings, a sweeping FOV wedge, glowing targets with velocity ticks, and
afterglow trails.

## 0.1.2 — 2026-08-04

Fixes an install that produced no app: the manifest declared no launcher
activity, so the APK installed and reported as present while having no
home-screen entry.

## 0.1.1 — 2026-08-04

Network hardening from a code review. The WebSocket reader now uses a dial
timeout, a read timeout, and keepalive pings, so a dead link is detected and
reconnected instead of parking a thread forever; the Live tab says "live" only
when data is actually arriving, and shows why the link is quiet otherwise.
Stop-recording is always available and only clears when the server confirms.

## 0.1.0 — 2026-08-04

Initial release: an honest calibration companion for the RuView LD2450/CSI
work. Live room map showing exactly what the sensing server reports — CSI
localization blob (radius = inverse confidence), mount-transformed LD2450
radar targets with trails, raw numeric readouts. Record tab drives
calibration-session recording and the empty-room baseline from the phone;
Settings persists the surveyed geometry (room, nodes, solve_mount pose).
