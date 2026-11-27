//! Unit tests of cursor files.

use super::*;
use crate::fs::MemFs;

fn p(sequence: u64) -> Position {
    Position {
        generation: 0,
        sequence,
    }
}

fn state(position: u64) -> CursorState {
    CursorState {
        consumer: "alarms".into(),
        position: p(position),
        latches: vec![
            Latch {
                funnel: 2,
                incarnation: p(3),
                emitted: p(9),
                state: LatchState::Active(true),
            },
            Latch {
                funnel: 4,
                incarnation: p(5),
                emitted: p(11),
                state: LatchState::Value(Some(2.5)),
            },
        ],
        reset: Some(p(7)),
        gate: None,
    }
}

#[test]
fn the_last_valid_block_is_the_state_and_a_torn_one_falls_back() {
    let fs = MemFs::new();
    let dir = Path::new("/s");
    let end = append(&fs, dir, [1; 16], 0, 5, &state(10), None).unwrap();
    let end2 = append(&fs, dir, [1; 16], 0, 5, &state(12), Some(end)).unwrap();
    let f = read(&fs, dir, &[1; 16], "alarms").unwrap().unwrap();
    assert_eq!((f.state.clone(), f.end), (state(12), end2));
    assert_eq!(f.state.highest(), p(12));
    // A torn last block leaves the older state.
    let file = path(dir, "alarms");
    fs.poke(&file, end as usize + BLOCK_HEADER_LEN + 2, &[0xff]);
    let f = read(&fs, dir, &[1; 16], "alarms").unwrap().unwrap();
    assert_eq!((f.state, f.end), (state(10), end));
    // No valid block: unreadable; another store's file too.
    fs.poke(&file, HEADER_LEN + 20, &[0xff]);
    assert!(matches!(
        read(&fs, dir, &[1; 16], "alarms"),
        Err(CursorError::Unreadable)
    ));
    assert!(read(&fs, dir, &[1; 16], "other").unwrap().is_none());
}

#[test]
fn a_large_file_is_written_again() {
    let fs = MemFs::new();
    let dir = Path::new("/s");
    let end = append(&fs, dir, [1; 16], 0, 5, &state(1), None).unwrap();
    let again = append(&fs, dir, [1; 16], 0, 5, &state(2), Some(REWRITE_AT)).unwrap();
    assert_eq!(again, end);
    assert_eq!(
        read(&fs, dir, &[1; 16], "alarms").unwrap().unwrap().state,
        state(2)
    );
    assert!(
        fs.paths()
            .iter()
            .all(|p| !p.to_string_lossy().ends_with(".new"))
    );
}

#[test]
fn an_unknown_critical_field_is_a_version_refusal() {
    let mut body = Vec::new();
    put_pos(&mut body, p(1));
    body.extend_from_slice(&[0; 4]);
    field(&mut body, 1, b"alarms");
    field(&mut body, 9, &[1]);
    let mut bytes = encode_header([1; 16], 0, 0).to_vec();
    frame(CURSOR_BLOCK, 0, &body, &mut bytes).unwrap();
    assert!(matches!(
        decode(&bytes, &[1; 16]),
        Err(CursorError::Unsupported(_))
    ));
}

#[test]
fn the_source_reports_the_highest_position_and_missing_or_unreadable_files() {
    let fs = MemFs::new();
    let dir = Path::new("/s");
    append(&fs, dir, [1; 16], 0, 5, &state(10), None).unwrap();
    let mut broken = state(1);
    broken.consumer = "broken".into();
    append(&fs, dir, [1; 16], 0, 5, &broken, None).unwrap();
    fs.poke(&path(dir, "broken"), HEADER_LEN + 20, &[0xff]);
    let consumers: BTreeSet<String> = ["alarms", "gone"].map(String::from).into();
    let r = CursorSource
        .read(&SourceContext {
            fs: &fs,
            store_dir: dir,
            store_id: [1; 16],
            generation: 0,
            consumers: &consumers,
        })
        .unwrap();
    assert_eq!(r.highest, 11);
    assert_eq!(r.named, vec!["broken".to_string(), "gone".to_string()]);
}

fn gated(position: u64, gate: GateCursor) -> CursorState {
    CursorState {
        gate: Some(gate),
        ..state(position)
    }
}

fn levels_mid_hold() -> Checkpoint {
    Checkpoint {
        through: p(20),
        now: 5_000,
        instances: vec![CheckpointInstance {
            gate: 4,
            series: 9,
            state: CheckpointState::Level(0),
            set_by: GateId {
                at: p(12),
                due: None,
                gate: 4,
                series: 9,
            },
            reseeded: false,
            crossed: 0,
            pending: vec![(0, 4_000)],
            reference: None,
            last_eval: 4_500,
        }],
        series: vec![CheckpointSeries {
            series: 9,
            live: Some(CheckpointFact {
                t: 4_000,
                position: p(18),
                value: CheckpointValue::Num(45.0),
                usable: true,
                changed_at: None,
            }),
            last_arrival: Some(4_000),
        }],
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn gate_cursor_vectors_match_format_md() {
    let id = GateCursor::Transition(GateId {
        at: p(12),
        due: Some(7_000),
        gate: 4,
        series: 9,
    });
    let mut v = Vec::new();
    checkpoint::put_point(&mut v, &id);
    assert_eq!(
        hex(&v),
        concat!(
            "01010000",                 // tag 1 transition, due present
            "000000000c00000000000000", // at (0, 12)
            "581b000000000000",         // due 7,000
            "04000000",                 // gate 4
            "09000000",                 // series 9
        )
    );
    let mut v = Vec::new();
    checkpoint::put_point(
        &mut v,
        &GateCursor::Frontier {
            at: p(20),
            now: 5_000,
        },
    );
    assert_eq!(
        hex(&v),
        concat!(
            "02010000",                 // tag 2 frontier, instant present
            "000000001400000000000000", // at (0, 20)
            "8813000000000000",         // now 5,000
            "0000000000000000",         // gate and series zero
        )
    );
    for g in [
        id,
        GateCursor::Nothing,
        GateCursor::Frontier {
            at: p(20),
            now: 5_000,
        },
    ] {
        let mut b = encode_header([1; 16], 0, 0).to_vec();
        b.extend(encode_block(&gated(10, g)).unwrap());
        assert_eq!(decode(&b, &[1; 16]).unwrap().state.gate, Some(g));
    }
}

#[test]
fn checkpoint_vector_matches_format_md_and_round_trips() {
    let body = checkpoint::encode_body(&levels_mid_hold()).unwrap();
    assert_eq!(
        hex(&body),
        concat!(
            "000000001400000000000000", // through (0, 20)
            "8813000000000000",         // now 5,000
            "01000000",                 // one instance
            "01000000",                 // one series
            "00000000",                 // reserved
            "04000000",                 // gate 4
            "09000000",                 // series 9
            "01000000",                 // set_by: tag 1, no due
            "000000000c00000000000000", // at (0, 12)
            "0000000000000000",         // due absent
            "04000000",                 // gate 4
            "09000000",                 // series 9
            "0000",                     // crossed none
            "01",                       // one pending hold
            "02",                       // state tag 2, levels
            "9411000000000000",         // last evaluation 4,500
            "00a00f000000000000",       // boundary 0 held since 4,000
            "00",                       // level 0
            "00",                       // no reference
            "09000000",                 // series 9
            "07",                       // live, last arrival, usable
            "a00f000000000000",         // last arrival 4,000
            "a00f000000000000",         // live t 4,000
            "000000001200000000000000", // live position (0, 18)
            "010000000000804640",       // number 45.0
        )
    );
    assert_eq!(checkpoint::decode_body(&body), Some(levels_mid_hold()));
}

#[test]
fn a_reseeded_instance_sets_byte_2_of_its_set_by_and_round_trips() {
    let mut c = levels_mid_hold();
    c.instances[0].reseeded = true;
    let body = checkpoint::encode_body(&c).unwrap();
    let plain = checkpoint::encode_body(&levels_mid_hold()).unwrap();
    assert_eq!(hex(&body[40..44]), "01000100");
    assert_eq!((&body[..42], &body[43..]), (&plain[..42], &plain[43..]));
    assert_eq!(checkpoint::decode_body(&body), Some(c));
    let mut bad = body.clone();
    bad[42] = 2;
    assert_eq!(checkpoint::decode_body(&bad), None);
    bad[42] = 0;
    bad[43] = 1;
    assert_eq!(checkpoint::decode_body(&bad), None);
}

fn with_checkpoint(fs: &MemFs, dir: &Path, c: &Checkpoint, g: GateCursor, end: Option<u64>) -> u64 {
    let st = gated(10, g);
    let rec = Record {
        state: &st,
        checkpoint: Some(c),
        fresh: true,
    };
    append_record(fs, dir, [1; 16], 0, 5, rec, end).unwrap()
}

#[test]
fn a_write_torn_in_its_checkpoint_leaves_the_previous_pair() {
    let fs = MemFs::new();
    let dir = Path::new("/s");
    let first = levels_mid_hold();
    let end = with_checkpoint(&fs, dir, &first, GateCursor::Nothing, None);
    let mut second = levels_mid_hold();
    second.through = p(30);
    let at12 = GateCursor::Transition(second.instances[0].set_by);
    let end2 = with_checkpoint(&fs, dir, &second, at12, Some(end));
    let f = read(&fs, dir, &[1; 16], "alarms").unwrap().unwrap();
    assert_eq!(
        (f.checkpoint.as_ref(), f.state.gate, f.end),
        (Some(&second), Some(at12), end2)
    );
    fs.poke(
        &path(dir, "alarms"),
        end as usize + BLOCK_HEADER_LEN + 3,
        &[0xff],
    );
    let f = read(&fs, dir, &[1; 16], "alarms").unwrap().unwrap();
    assert_eq!(
        (f.checkpoint, f.state.gate, f.end),
        (Some(first), Some(GateCursor::Nothing), end)
    );
}

#[test]
fn a_write_torn_in_its_cursor_block_keeps_the_new_checkpoint_under_the_old_block() {
    let fs = MemFs::new();
    let dir = Path::new("/s");
    let end = with_checkpoint(&fs, dir, &levels_mid_hold(), GateCursor::Nothing, None);
    let mut second = levels_mid_hold();
    second.through = p(30);
    let ck_len = {
        let mut b = Vec::new();
        frame(
            CHECKPOINT_BLOCK,
            0,
            &checkpoint::encode_body(&second).unwrap(),
            &mut b,
        )
        .unwrap();
        b.len() as u64
    };
    let at12 = GateCursor::Transition(second.instances[0].set_by);
    with_checkpoint(&fs, dir, &second, at12, Some(end));
    fs.poke(
        &path(dir, "alarms"),
        (end + ck_len) as usize + BLOCK_HEADER_LEN + 3,
        &[0xff],
    );
    let f = read(&fs, dir, &[1; 16], "alarms").unwrap().unwrap();
    assert_eq!(
        (f.checkpoint, f.state.gate, f.end),
        (Some(second), Some(GateCursor::Nothing), end + ck_len)
    );
}

#[test]
fn a_file_written_again_carries_the_checkpoint() {
    let fs = MemFs::new();
    let dir = Path::new("/s");
    let c = levels_mid_hold();
    let end = with_checkpoint(&fs, dir, &c, GateCursor::Nothing, None);
    let st = gated(11, GateCursor::Nothing);
    let rec = Record {
        state: &st,
        checkpoint: Some(&c),
        fresh: false,
    };
    let plain = append_record(&fs, dir, [1; 16], 0, 5, rec, Some(end)).unwrap();
    assert_eq!(
        plain - end,
        encode_block(&st).unwrap().len() as u64,
        "carried, not appended"
    );
    append_record(&fs, dir, [1; 16], 0, 5, rec, Some(REWRITE_AT)).unwrap();
    let f = read(&fs, dir, &[1; 16], "alarms").unwrap().unwrap();
    assert_eq!((f.checkpoint, f.state), (Some(c), st));
}

#[test]
fn a_cursor_file_without_gate_fields_decodes() {
    let bytes = {
        let mut b = encode_header([1; 16], 0, 5).to_vec();
        b.extend(encode_block(&state(10)).unwrap());
        b
    };
    // The block body as written before field 4 and block 0x31 existed.
    let body = &bytes[HEADER_LEN + BLOCK_HEADER_LEN..];
    assert_eq!(
        hex(&body[..132]),
        concat!(
            "000000000a000000000000000000000001000600616c61726d73000002002800",
            "0200000000000000030000000000000000000000090000000000000001000000",
            "0000000000000000020028000400000000000000050000000000000000000000",
            "0b0000000000000003000000000000000000044003000c000000000007000000",
            "00000000",
        )
    );
    let f = decode(&bytes, &[1; 16]).unwrap();
    assert_eq!((f.state, f.checkpoint), (state(10), None));
}

#[test]
fn an_unknown_critical_block_type_is_a_version_refusal() {
    let mut bytes = encode_header([1; 16], 0, 0).to_vec();
    bytes.extend(encode_block(&state(1)).unwrap());
    frame(0x32, 0, &[0; 8], &mut bytes).unwrap();
    assert!(matches!(
        decode(&bytes, &[1; 16]),
        Err(CursorError::Unsupported(_))
    ));
}

#[test]
fn a_damaged_checkpoint_or_gate_point_is_not_read() {
    let body = checkpoint::encode_body(&levels_mid_hold()).unwrap();
    // Counts beyond the body, a trailing byte, an unknown state tag, a frontier naming a gate.
    let mut big = body.clone();
    big[20] = 0xff;
    let mut trailing = body.clone();
    trailing.push(0);
    let mut tag = body.clone();
    tag[75] = 9;
    for b in [big, trailing, tag, body[..body.len() - 1].to_vec()] {
        assert_eq!(checkpoint::decode_body(&b), None);
    }
    let mut v = Vec::new();
    checkpoint::put_point(&mut v, &GateCursor::Frontier { at: p(1), now: 2 });
    v[24] = 1;
    assert_eq!(checkpoint::point_at(&v), None);
}
