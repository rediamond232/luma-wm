---
version: alpha
name: Luma shell
description: A quiet, wallpaper-toned control surface for a fast native desktop.
colors:
  background: "#1b1b14"
  foreground: "#fff3d1"
  accent: "#e7bd58"
  muted: "#aeb286"
  primary: "{colors.accent}"
typography:
  interface:
    fontFamily: "Noto Sans, sans-serif"
    fontSize: "14px"
  utility:
    fontFamily: "Noto Sans Mono, monospace"
rounded:
  panel: "14px"
  control: "8px"
spacing:
  bar-inset: "4px"
  bar-chip-gap: "6px"
  panel-padding: "24px"
components:
  bar:
    backgroundColor: "{colors.background}"
    textColor: "{colors.foreground}"
  workspace-active:
    backgroundColor: "{colors.accent}"
    textColor: "{colors.background}"
  launcher:
    backgroundColor: "{colors.background}"
    textColor: "{colors.foreground}"
    rounded: "{rounded.panel}"
  control-panel:
    backgroundColor: "{colors.background}"
    textColor: "{colors.foreground}"
  notification:
    backgroundColor: "{colors.background}"
    textColor: "{colors.muted}"
  lock-screen:
    backgroundColor: "{colors.background}"
    textColor: "{colors.foreground}"
---

# Luma shell design system

## Overview

Luma is a native desktop shell for someone who uses a high refresh monitor for
games and everyday work. The shell should be legible at a glance, make launch
and system controls easy to find, and stay quiet during fullscreen play.

The direction takes the segmented bar and centered launcher structure of
[Caelestia](https://github.com/caelestia-dots/shell) and the wallpaper-toned
surfaces of [end-4](https://github.com/end-4/dots-hyprland). Luma's signature is
a small gold launcher tile followed by a workspace ribbon, with separate
floating islands for the active window and status controls. Gold identifies
selection and action; it does not outline every piece of content.

The shell is a product surface, not a wallpaper showcase. Keep the active app
dominant. Avoid a continuous full-width opaque bar, permanent dashboards,
animated visualizers, and decorative motion that redraws while idle.

`config/default.toml` owns the runtime palette, type size, opacity, radius,
and shell height. `wm_core::Config` parses it, and its `Theme`/`Shell` defaults
mirror the same values when no config file is available. The native shell maps
those values through `Colors::from_config` and shared drawing primitives in
`crates/shell-sctk/src/main.rs`; the GTK fallback maps the same values through
`css()` in `crates/shell/src/main.rs`. This file mirrors those canonical
values and records how to use them. When a shared value changes, update the
config and this file together.

## Colors

The active wallpaper has sunlit yellow and olive shapes. The current palette
uses charcoal olive `background`, cream `foreground`, gold `accent`, and sage
`muted` so the shell belongs with it. Text and controls use semantic roles from
the four config colors; this spec's `primary` aliases `accent`. A selected
workspace may fill with accent; ordinary status uses muted text on the
background surface. Errors and recording state
must also have a readable label, never color alone.

Native SHM colors are premultiplied ARGB. Tint existing colors with
`Colors::scaled` instead of copying hex values into each draw function.

## Typography

Noto Sans at 14 px carries the bar, panels, and launcher. Use stronger size and
placement for panel headings, not all caps everywhere. Noto Sans Mono is an
optional utility face for numeric readouts; it is not a new renderer dependency.
Names of windows and apps may be long or mixed script, so clip them inside
their own island without moving other controls.

## Layout

The 42 px bar reserves space for three groups: launcher and workspaces at the
left, current window in the available middle, and status chips from the right
edge. Four pixel vertical insets and six pixel chip gaps expose the desktop
between groups. Drawing and pointer hit testing share the same geometry.

The launcher stays a centered 680 × 420 layer. Popovers remain bounded near
the related bar control. Open panels can use more space than the bar, but keep
clear headings, consistent row rhythm, and footer guidance. A narrow output
omits the title and then lower-priority status chips before overlapping the
workspace ribbon.

## Elevation & Depth

Use near-opaque theme surfaces, a quiet border, and small inset fills to mark
layers. Backdrop blur is disabled by default. The configured two-pass ceiling
remains available if a user explicitly enables it; the persistent bar still
skips the backdrop shader. Fullscreen content keeps its existing fast path.

## Shapes

Large surfaces use the theme's 14 px radius. Search fields, selected rows, and
workspace cells use a tighter 8 px radius. Controls share those primitives;
avoid a collection of unrelated circles, pill radii, and stroke weights.

## Components

### Bar

The gold launcher tile opens search. The active workspace is a filled cell and
has a short gold marker; inactive workspaces stay quiet. Each status chip has
one clear hit area and a clipped label. Recording and Do Not Disturb have a
visible text state. Media and network chips show short status labels; their
panels carry the track and connection names. Notifications show a count.

### Launcher and controls

The launcher has one search field, one results area, a visible selection, and
plain keyboard hints. Controls use the same heading, inset row, selection, and
footer grammar. Actions keep their existing keyboard and pointer behavior.

### Notifications

Use a distinct summary and secondary body. Keep critical warnings persistent;
ordinary notifications expire under the existing five-second rule. A notice
must fit its bounded surface without pushing other shell modules.

### Lock screen and legacy titlebars

The standalone Hyprlock theme uses the same charcoal, cream, olive, and gold
palette with a plain background and one password field. Keep it free of blur,
animated content, and shell commands. The small server-side titlebar for
legacy windows uses the same default palette and keeps its existing hit areas.

### Motion and resource use

Redraw on state changes and direct interaction. Pause wallpaper video under
the existing fullscreen and battery rules. Do not add an idle frame loop,
continuous audio visualizer, always-live window previews, or a browser UI.
Respect the existing reduced-motion setting for any future transition.

## Do's and Don'ts

- Do keep the shell's colors tied to the wallpaper through the config theme.
- Do use shared native geometry for visible controls and their hit targets.
- Don't trade away the fullscreen fast path to decorate the shell.
- Don't use blur or animation as a substitute for spacing and hierarchy.
