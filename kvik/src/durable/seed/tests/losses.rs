//! The comparison of loss reports across starts that may number a loss differently (Q-200, Q-1115).

/// `line` without each loss report's identity, segment and region size, which follow the region met first.
pub(super) fn without_identity(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    loop {
        let cut = ["identity: Position {", "file_id: ", "bytes: "]
            .iter()
            .filter_map(|k| rest.find(k).map(|i| (i, *k)))
            .min();
        let Some((i, key)) = cut else { break };
        out.push_str(&rest[..i]);
        let tail = &rest[i + key.len()..];
        let end = if key != "identity: Position {" {
            tail.find(", ").map_or(tail.len(), |n| n + 2)
        } else {
            tail.find("}, ").map_or(tail.len(), |n| n + 3)
        };
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// Whether two starts report the same losses once identities are set aside, in any order.
pub(super) fn alike_without_identity(whole: &[String], seeded: &[String]) -> bool {
    whole
        .iter()
        .map(|l| without_identity(l))
        .eq(seeded.iter().map(|l| without_identity(l)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loss(identity: u64, first: u64) -> String {
        format!(
            "LossReport {{ identity: Position {{ generation: 0, sequence: {identity} }}, kind: DataLost, \
             first: Some({first}), bytes: 4096, recorded: true }}"
        )
    }

    #[test]
    fn losses_alike_whatever_order_their_identities_fall_in() {
        let whole = [loss(3, 10), loss(7, 20)];
        let seeded = [loss(2, 20), loss(5, 10)];
        assert!(alike_without_identity(&whole, &seeded));
    }

    #[test]
    fn a_lost_loss_is_not_alike() {
        assert!(!alike_without_identity(
            &[loss(3, 10), loss(7, 20)],
            &[loss(2, 20)]
        ));
    }

    #[test]
    fn a_duplicated_loss_is_not_alike() {
        let whole = [loss(3, 10), loss(7, 20)];
        let seeded = [loss(2, 10), loss(5, 10)];
        assert!(!alike_without_identity(&whole, &seeded));
    }
}
