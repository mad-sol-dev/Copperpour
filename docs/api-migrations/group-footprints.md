### New tool `group_footprints` in the `placement` toolset (minor release)

Copperpour could not create a KiCad group: `group_components` writes a symbol
field named `Group`, which KiCad never reads as a group. `group_footprints`
takes `board`, `references` and `name` and creates the board's own
`(group …)`, the block KiCad selects, moves and locks together. With the board
open in KiCad it is created over IPC in one undo step; otherwise the closed
board file gets it, and the response carries `fallback_reason` and `warning`
as `set_component_placements` does. A reference that is missing or repeated,
or a footprint that is already a member of a group, is refused as
`invalid_argument` naming `references[i]`, and nothing is written. The
response reports `group.uuid`, `group.name` and `group.members` as read back
after the write; a live readback that does not show the requested group is
`mutation_outcome_uncertain`.

`group_components` is unchanged; its description now says that it writes only
a field. No existing call changes.
