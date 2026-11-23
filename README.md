
# BS Data Utils

A collection of rust crates for use in beat saber-adjacent projects


## bs_mapping_data

Represents every file format as rust enums and structs.

This includes:
- Info
  - V2 (Including CustomData for SongCore, SettingsSetter, Beatcraft)
  - V4
- Audio Info
  - V2
  - V4
- Beatmap
  - V2 (Including CustomData for Tracks, Noodle, Chroma)
  - V3 (Including CustomData for Tracks, Noodle, Chroma)
  - V4
- Lightshow
  - V4

## beatmap_core

Beatmap and lightshow animation library.

Uses a best-guess/best-effort animation based on Arc-Viewer, ChroMapper,
and Beatcraft 0.9.4, and uses tracks-rs for animation/track support.

For beatmaps, the animation is mostly correct, though has a few minor
discrepancies where the base game has sudden jumps, but this does not.

light shows are not fully implemented yet, but will not work directly off
of raw lightshow data due to wanting this to be a "stateless" system.

The system being "stateless" just refers to the ability to instantly get
the animated position at any point in time, and not having to re-evaluate
entire event lanes when seeking backwards.

Note that obstacle sizes are not encoded into the animation matrix, and have
to be handled manually by the library user. The obstacle origin at the front
bottom of the wall, and exactly halfway across on the X axis.




