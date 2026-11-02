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
