# Footprint-owned outline fixtures

Fixtures for Copperpour/Copperpour#27: a board whose only `Edge.Cuts`
graphics are drawn inside a footprint. Both boards were written by KiCad
10.0.7's own pcbnew serializer, through its Python API, with
[`generate.py`](generate.py):

```bash
python3 generate.py .
```

Running it again gives the same geometry but new UUIDs, so the hashes below
change.

## Content

`MECH1` (`fixture:MECH_outline`) has no pads. It draws, in footprint
coordinates:

- an outer rounded rectangle 140 × 100 mm centred on its anchor, corner radius
  5 mm: 4 `fp_line` + 4 `fp_arc`;
- an internal rounded cutout 24.25 × 7.333 mm centred on (20, 10), corner
  radius 1 mm: 4 `fp_line` + 4 `fp_arc`;
- an `F.CrtYd` rectangle a little larger than the outline, so that treating
  MECH1 as a component makes its courtyard overlap every other part.

`R1` and `R2` are 0603 resistors (two SMD pads each, net `SIG`) at
(90, 80) and (110, 80) relative to a (100, 100) MECH1 anchor.

| File | MECH1 placement | SHA-256 |
| --- | --- | --- |
| `footprint_outline_front.kicad_pcb` | (100, 100), 0°, F.Cu | `320b6a0d9d7e5b45a7bbdc8307261166f21efcdfa0152dbe3e847893b5357f11` |
| `footprint_outline_back_rot30.kicad_pcb` | (100, 100), rotated 30°, flipped to B.Cu (pcbnew writes `(at 100 100 -30)`) | `33965a7f072e07e1633513fc7588e94504c33428a80535042dc54f3fe545b025` |

## Expected values (KiCad's, not Copperpour's)

Read back from the saved files with `BOARD::GetBoardPolygonOutlines`, which
turns arcs into chords, so arc extrema can sit a few micrometres inside the
exact ones:

| File | Outline bbox (mm) | Outlines / holes | Hole bbox (mm) |
| --- | --- | --- | --- |
| front | (30, 50)..(170, 150) | 1 / 1 | (107.875, 106.3335)..(132.125, 113.6665) |
| back, 30° | (16.21073, 23.531238)..(183.78927, 176.468784) | 1 / 1 | (110.350682, 92.466096)..(134.290328, 110.213389) |

These are not the MECH1 fixture offered in the issue (27 primitives), which
was never attached; they are minimal boards with the same structure, an outer
contour plus an internal cutout, made from lines and arcs.
