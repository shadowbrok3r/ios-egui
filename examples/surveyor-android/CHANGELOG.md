# Changelog

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
