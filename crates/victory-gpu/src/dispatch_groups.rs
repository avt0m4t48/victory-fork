/// The host's maximum group count in one dispatch dimension.
pub(crate) const MAX_GROUPS: u32 = 65_535;

/// Runs the X pieces of a direct dispatch. The base root slot is optional
/// because shaders which never read group ID X need no offset.
pub(crate) fn dispatch_groups(
    x: u32,
    split: bool,
    base_slot: Option<(u32, u32)>,
    mut dispatch: impl FnMut(u32, Option<((u32, u32), u32)>),
) {
    if !split || x <= MAX_GROUPS {
        dispatch(x.min(MAX_GROUPS), None);
        return;
    }

    for base in (0..x).step_by(MAX_GROUPS as usize) {
        let binding = base_slot.filter(|_| base != 0).map(|slot| (slot, base));
        dispatch((x - base).min(MAX_GROUPS), binding);
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_GROUPS, dispatch_groups};

    #[test]
    fn oversized_dispatch_runs_all_groups_without_a_base_slot() {
        let mut pieces = Vec::new();
        dispatch_groups(MAX_GROUPS + 2, true, None, |count, base| {
            pieces.push((count, base))
        });
        assert_eq!(pieces, [(MAX_GROUPS, None), (2, None)]);
    }

    #[test]
    fn dispatch_pieces_bind_only_nonzero_bases_when_splitting_is_enabled() {
        let slot = (2, 5);
        let mut pieces = Vec::new();
        dispatch_groups(2 * MAX_GROUPS + 1, true, Some(slot), |count, base| {
            pieces.push((count, base))
        });
        assert_eq!(
            pieces,
            [
                (MAX_GROUPS, None),
                (MAX_GROUPS, Some((slot, MAX_GROUPS))),
                (1, Some((slot, 2 * MAX_GROUPS)))
            ]
        );

        pieces.clear();
        dispatch_groups(MAX_GROUPS + 1, false, Some(slot), |count, base| {
            pieces.push((count, base))
        });
        assert_eq!(pieces, [(MAX_GROUPS, None)]);
    }
}
