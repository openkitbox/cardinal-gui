//! Pure reordering of the account cards the page currently shows into the
//! gateway's global key order.
//!
//! An account's `position` is one ordering over EVERY account, archived ones
//! included, and `PUT /api/keys/order` accepts only the complete id list (each
//! id exactly once). The 全部 view hides archived accounts, so a drag may only
//! move what the user can see: [`reorder_visible`] reorders the visible
//! subsequence and writes the result back into the slots those cards already
//! occupy, leaving every hidden account where it was. No I/O and no gpui
//! dependency — the page calls this and submits the returned list.

/// Which side of the drop target the dragged card lands on.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum DropSide {
    Before,
    After,
}

/// The global order after dropping `dragged` on the [`DropSide`] of `target`,
/// moving only the cards in `visible`.
///
/// `all` is the complete loaded order (archived accounts included), `visible`
/// the ids the current filter shows, in the same order. The visible slots of
/// `all` are refilled with the reordered subsequence; ids outside `visible`
/// keep their slot. `all` is returned unchanged when the drop cannot be
/// placed — dropping a card on itself, an id missing from `visible`, or a
/// `visible` that is not a subsequence of `all` (fail closed, so the caller
/// never submits an order it could not derive).
#[must_use]
pub(super) fn reorder_visible(
    all: &[i64],
    visible: &[i64],
    dragged: i64,
    target: i64,
    side: DropSide,
) -> Vec<i64> {
    if dragged == target
        || !visible.contains(&dragged)
        || !visible.contains(&target)
        || !is_subsequence(all, visible)
    {
        return all.to_vec();
    }
    // The visible order without the dragged card: the target keeps its slot
    // there, so the drop side is decided against the list the card is about
    // to join.
    let mut ordered: Vec<i64> = visible
        .iter()
        .copied()
        .filter(|id| *id != dragged)
        .collect();
    let Some(anchor) = ordered.iter().position(|id| *id == target) else {
        // Unreachable today: the early return above already covers a target
        // equal to the dragged id, and account ids are primary keys. Kept as a
        // fail-closed arm so a future caller passing a repeated id can never
        // make this panic or shift the wrong slot.
        return all.to_vec();
    };
    let insert_at = match side {
        DropSide::Before => anchor,
        DropSide::After => anchor + 1,
    };
    ordered.insert(insert_at, dragged);
    // Write the reordered subsequence back into the slots the visible cards
    // occupy; archived ids never move. A slot count that disagrees with the
    // visible list means the two lists do not describe the same cards, so the
    // order is returned untouched rather than shorter or padded.
    let slots: Vec<usize> = all
        .iter()
        .enumerate()
        .filter(|(_, id)| visible.contains(id))
        .map(|(slot, _)| slot)
        .collect();
    if slots.len() != visible.len() {
        return all.to_vec();
    }
    let mut next = all.to_vec();
    for (slot, id) in slots.into_iter().zip(ordered) {
        next[slot] = id;
    }
    next
}

/// Whether `visible` appears inside `all` in the same relative order — the
/// invariant that makes the slot write-back lossless.
fn is_subsequence(all: &[i64], visible: &[i64]) -> bool {
    let mut remaining = all.iter();
    visible.iter().all(|id| remaining.any(|other| other == id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moving_down_lands_the_card_after_its_target() {
        let all = [1, 2, 3];
        let moved = reorder_visible(&all, &all, 1, 3, DropSide::After);
        assert_eq!(moved, [2, 3, 1], "dropping 1 after 3 puts it last");
    }

    #[test]
    fn moving_up_lands_the_card_before_its_target() {
        let all = [1, 2, 3];
        let moved = reorder_visible(&all, &all, 3, 1, DropSide::Before);
        assert_eq!(moved, [3, 1, 2], "dropping 3 before 1 puts it first");
    }

    #[test]
    fn archived_accounts_keep_their_slots() {
        // 10 and 11 are archived: the 全部 view never shows them.
        let all = [10, 1, 2, 11, 3];
        let visible = [1, 2, 3];
        let moved = reorder_visible(&all, &visible, 3, 1, DropSide::Before);
        assert_eq!(
            moved,
            [10, 3, 1, 11, 2],
            "only the visible slots 1, 2, 3 move; the archived ids 10 and 11 stay in place"
        );
    }

    #[test]
    fn dropping_a_card_on_itself_changes_nothing() {
        let all = [10, 1, 2, 11, 3];
        let visible = [1, 2, 3];
        assert_eq!(
            reorder_visible(&all, &visible, 2, 2, DropSide::Before),
            all,
            "a card cannot move relative to itself"
        );
        assert_eq!(
            reorder_visible(&all, &visible, 2, 2, DropSide::After),
            all,
            "the insertion side is irrelevant for a card dropped on itself"
        );
    }

    #[test]
    fn a_target_outside_the_visible_list_changes_nothing() {
        let all = [10, 1, 2, 11, 3];
        let visible = [1, 2, 3];
        assert_eq!(
            reorder_visible(&all, &visible, 1, 10, DropSide::After),
            all,
            "an archived target has no slot in the visible order to anchor the drop"
        );
    }

    #[test]
    fn a_dragged_card_outside_the_visible_list_changes_nothing() {
        let all = [10, 1, 2, 11, 3];
        let visible = [1, 2, 3];
        assert_eq!(
            reorder_visible(&all, &visible, 11, 1, DropSide::Before),
            all,
            "an archived drag source has no visible slot to move"
        );
    }

    #[test]
    fn a_visible_list_that_is_not_a_subsequence_is_refused() {
        let all = [1, 2, 3];
        assert_eq!(
            reorder_visible(&all, &[2, 1, 3], 3, 2, DropSide::Before),
            all,
            "the filter order disagreeing with the global order means the two lists are out of sync, so the drop is refused rather than guessed"
        );
        assert_eq!(
            reorder_visible(&all, &[1, 2, 99], 99, 1, DropSide::Before),
            all,
            "a visible id the global order does not contain cannot be written back into a slot"
        );
    }

    #[test]
    fn a_global_order_that_repeats_a_visible_id_is_refused() {
        // Account ids are primary keys, so the gateway never sends this; the
        // guard exists so a malformed list can never come back as a different
        // length than it went in.
        let all = [1, 1, 2];
        assert_eq!(
            reorder_visible(&all, &[1, 2], 1, 2, DropSide::After),
            all,
            "two slots claim the same visible card, so the write-back cannot stay slot-for-slot"
        );
    }

    #[test]
    fn a_single_visible_card_changes_nothing() {
        let all = [10, 1, 11];
        let visible = [1];
        assert_eq!(
            reorder_visible(&all, &visible, 1, 1, DropSide::After),
            all,
            "one card has no other visible card to trade places with"
        );
    }

    #[test]
    fn a_full_view_reorders_into_a_permutation_of_the_same_length() {
        let all = [1, 2, 3, 4];
        let moved = reorder_visible(&all, &all, 4, 2, DropSide::Before);
        assert_eq!(moved, [1, 4, 2, 3], "4 lands before 2");
        let mut sorted = moved.clone();
        sorted.sort_unstable();
        assert_eq!(
            sorted, all,
            "a reorder must return every id exactly once — the gateway rejects a list with a gap or a duplicate"
        );
    }

    #[test]
    fn dropping_where_the_card_already_sits_changes_nothing() {
        // The page skips the write when the result equals the input, so a drop
        // on the side a card already occupies must come back element-for-
        // element equal — otherwise every no-op drag would cost a request.
        let all = [1, 2, 3];
        assert_eq!(
            reorder_visible(&all, &all, 2, 1, DropSide::After),
            all,
            "2 already sits right after 1, so the drop asks for the order it has"
        );
        assert_eq!(
            reorder_visible(&all, &all, 2, 3, DropSide::Before),
            all,
            "2 already sits right before 3, so the drop asks for the order it has"
        );
    }

    #[test]
    fn adjacent_visible_cards_between_archived_ones_still_move() {
        // Visible order decides, not global adjacency: moving 1 after 3 swaps
        // them even though an archived account sits between their slots.
        let all = [10, 1, 11, 3];
        let visible = [1, 3];
        assert_eq!(
            reorder_visible(&all, &visible, 1, 3, DropSide::After),
            [10, 3, 11, 1],
            "the visible cards swap, and the archived id keeps its own slot"
        );
    }
}
