//! Two joint types hold a part against a cylinder, as the Mate tool (`assembly.mate`) makes them:
//! `line_on_cylinder` (a line parallel to the cylinder's axis, at `offset` from it: a cylinder
//! resting on another, side by side, or a straight edge on a cylinder) and `point_on_cylinder` (a
//! point at `offset` from the axis). A joint's `type` may be either; nothing else is saved
//! differently, and older designs are read as they were. The last release warns about the newer
//! format number and opens the design without its assembly section (its joints, joint origins
//! and motion links) when a joint has one of these types; it keeps that section and writes it
//! back unchanged when the design is saved there.
