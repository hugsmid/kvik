//! The stored levels (design 7.1b section 2): one table, shared by writer, reader and model. Files carry level ids
//! and repeat the table they were written under, so changing this table changes no byte layout.

/// One minute in nanoseconds; entry gaps and level widths are whole minutes (`format-tree.md` T.9).
pub const MINUTE: i64 = 60_000_000_000;

/// A stored level: its id in files and the width of its nodes, UTC-aligned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Level {
    /// Level id (`format-tree.md` T.11); 16 and 17 are reserved for calendar levels (Q-1425).
    pub id: u8,
    /// Node width in nanoseconds: a positive whole number of minutes.
    pub width: i64,
}

/// The levels written now: 1 minute (Q-1420), 15 minutes, and the 2-hour and 1-day roll-ups (Q-1421).
pub const LEVELS: [Level; 4] = [
    Level {
        id: 1,
        width: MINUTE,
    },
    Level {
        id: 2,
        width: 15 * MINUTE,
    },
    Level {
        id: 3,
        width: 120 * MINUTE,
    },
    Level {
        id: 4,
        width: 1_440 * MINUTE,
    },
];
