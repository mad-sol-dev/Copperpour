### `score_placement` reads an outline drawn inside footprints (minor release)

A board with no top-level `Edge.Cuts` graphic, whose outline is drawn as
`Edge.Cuts` inside one or more footprints, used to score as
`outline_missing`, and a padless outline footprint's courtyard counted as a
component that overlapped every other part. When those footprint graphics
chain into closed contours (endpoints within 0.001 mm), `score_placement` now
uses them as the outline, so `outline_missing` is `false` and the verdict
follows `outline_shape` as for any other outline. A padless footprint that
owns that outline is left out of the overlap, outside-outline, connector and
decoupling checks, and of `footprints_scored`.

New response fields: `outline_source` (`"board"`, `"footprint"` or
`"missing"`), `outline_owners` (references of footprints that draw
`Edge.Cuts`, also filled when their graphics were rejected),
`outline_contours` (`primitives`, `closed`, `open`, `cutouts`,
`tolerance_mm`), `outline_carriers_excluded` and `warnings`. Boards with a
top-level outline score as before and report `outline_source: "board"`.

The placement planners (`place_decoupling_caps`, `auto_place_from_schematic`,
`refine_placement_force_directed`) still read only a top-level outline. The
board file is never changed; moving a footprint-owned outline to top-level
graphics stays a manual step.
