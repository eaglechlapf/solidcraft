//! Three joint types the Mate tool (`assembly.mate`) makes: `line_on_cylinder` (a line parallel
//! to a cylinder's axis, at `offset` from it: a cylinder resting on another, side by side, or a
//! straight edge on a cylinder), `point_on_cylinder` (a point at `offset` from the axis) and
//! `angle_range` (two faces at an angle that swings between its value's limits). A joint's
//! `type` may be any of them; nothing else is saved differently, and older designs are read as
//! they were. The last release warns about the newer format number and opens the design without
//! its assembly section (its joints, joint origins and motion links) when a joint has one of these
//! types; it keeps that section and writes it back unchanged when the design is saved there.
