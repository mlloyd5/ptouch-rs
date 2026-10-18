// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Sequence membership is independent of composition order. Legacy documents
//! keep their adjacent-relative semantics until an explicit structural edit.

use std::collections::BTreeMap;

use crate::{
    RenderError, Result,
    document::{LabelElement, PositionMode, TextBox},
    object_layout::{Placement, WidthSizing},
};

#[derive(Default)]
pub struct LayoutCursor {
    pub right_mm: f64,
    sequences: BTreeMap<u64, f64>,
}

impl LayoutCursor {
    pub fn place(&mut self, frame: &TextBox, sequence: Option<u64>) -> Result<f64> {
        let x = match frame.position {
            PositionMode::Absolute => frame.x_mm,
            PositionMode::Relative => {
                let previous = match sequence {
                    Some(id) => *self.sequences.get(&id).ok_or_else(|| {
                        RenderError::Layout("sequence must start at an absolute anchor".into())
                    })?,
                    None => self.right_mm,
                };
                previous + frame.gap_mm
            }
        };
        self.right_mm = x + frame.width_mm;
        if let Some(id) = sequence {
            self.sequences.insert(id, self.right_mm);
        }
        Ok(x)
    }
}

pub fn validate(elements: &[LabelElement], version: u32) -> Result<()> {
    let mut seen = BTreeMap::new();
    for object in elements.iter().filter_map(LabelElement::as_object) {
        let Some(id) = object.sequence else { continue };
        if version < 11 || id == 0 || id > i64::MAX as u64 {
            return Err(RenderError::Layout(
                "sequence identifiers require version 11 and must be in 1..=i64::MAX".into(),
            ));
        }
        if object
            .layout
            .as_ref()
            .is_some_and(|p| p.placement == Placement::Free)
        {
            return Err(RenderError::Layout(
                "free objects cannot belong to a sequence".into(),
            ));
        }
        let expected = if seen.insert(id, ()).is_some() {
            PositionMode::Relative
        } else {
            PositionMode::Absolute
        };
        if object.frame.position != expected {
            return Err(RenderError::Layout(
                "a sequence requires one absolute anchor followed by relative members".into(),
            ));
        }
    }
    Ok(())
}

/// Explicit members can have independent objects between them in the list.
fn attached(element: &LabelElement) -> bool {
    element.as_object().is_some_and(|object| {
        object
            .scene
            .as_ref()
            .is_some_and(|scene| scene.anchor.is_some())
    })
}

pub fn members(elements: &[LabelElement], index: usize) -> Option<Vec<usize>> {
    let object = elements.get(index)?.as_object()?;
    if attached(&elements[index]) {
        return None;
    }
    if let Some(id) = object.sequence {
        return Some(
            elements
                .iter()
                .enumerate()
                .filter_map(|(i, e)| (e.as_object()?.sequence == Some(id)).then_some(i))
                .collect(),
        );
    }
    let active: Vec<_> = elements
        .iter()
        .enumerate()
        .filter_map(|(i, e)| (!attached(e)).then_some(i))
        .collect();
    let mut start = active.iter().position(|i| *i == index)?;
    while start > 0
        && elements[active[start]].as_object()?.frame.position == PositionMode::Relative
        && elements[active[start - 1]]
            .as_object()
            .is_some_and(|o| o.sequence.is_none())
    {
        start -= 1;
    }
    let mut end = start;
    while active
        .get(end + 1)
        .and_then(|i| elements[*i].as_object())
        .is_some_and(|o| o.sequence.is_none() && o.frame.position == PositionMode::Relative)
    {
        end += 1;
    }
    let first = elements[active[start]].as_object()?;
    (end > start
        || first.frame.position == PositionMode::Relative
        || first
            .layout
            .as_ref()
            .is_some_and(|p| p.placement == Placement::Flow))
    .then(|| active[start..=end].to_vec())
}

/// Preserve exact physical origins while recording previously implicit links.
pub fn materialize(elements: &mut [LabelElement]) -> Result<()> {
    if elements.iter().any(|e| e.as_object().is_none()) {
        return Err(RenderError::Layout(
            "convert legacy elements before editing sequence membership".into(),
        ));
    }
    let mut cursor = LayoutCursor::default();
    let positions: Vec<_> = elements
        .iter()
        .map(|e| {
            let o = e.as_object().unwrap();
            if attached(e) {
                Ok(o.frame.x_mm)
            } else {
                cursor.place(&o.frame, o.sequence)
            }
        })
        .collect::<Result<_>>()?;
    let mut next = elements
        .iter()
        .filter_map(|e| e.as_object()?.sequence)
        .max()
        .unwrap_or(0);
    let mut index = 0;
    while index < elements.len() {
        if elements[index].as_object().unwrap().sequence.is_some() {
            index += 1;
            continue;
        }
        if let Some(chain) = members(elements, index) {
            next = next
                .checked_add(1)
                .ok_or_else(|| RenderError::Layout("sequence identifier limit reached".into()))?;
            for &i in &chain {
                let object = elements[i].as_object_mut().unwrap();
                object.sequence = Some(next);
                if let Some(policy) = &mut object.layout
                    && policy.placement == Placement::Free
                {
                    policy.placement = Placement::Flow;
                    policy.width = WidthSizing::Fixed;
                }
            }
            let start = chain[0];
            let frame = &mut elements[start].as_object_mut().unwrap().frame;
            frame.position = PositionMode::Absolute;
            frame.x_mm = positions[start];
            index = *chain.last().unwrap() + 1;
        } else {
            index += 1;
        }
    }
    Ok(())
}

fn anchors(elements: &[LabelElement]) -> BTreeMap<u64, f64> {
    let mut anchors = BTreeMap::new();
    for e in elements {
        if let Some(o) = e.as_object()
            && let Some(id) = o.sequence
        {
            anchors.entry(id).or_insert(o.frame.x_mm);
        }
    }
    anchors
}

/// Before v13 a head's gap was inactive scratch data. Initialize it once,
/// before structural editing, without moving any allocation. In v13 it is
/// dormant incoming spacing and survives promotion, demotion and persistence.
fn initialize_head_spacing(elements: &mut [LabelElement]) {
    let mut seen = BTreeMap::new();
    for index in 0..elements.len() {
        let Some(object) = elements[index].as_object() else {
            continue;
        };
        let Some(id) = object.sequence else { continue };
        if seen.insert(id, ()).is_some() {
            continue;
        }
        let gap = elements[index + 1..]
            .iter()
            .filter_map(LabelElement::as_object)
            .find(|object| object.sequence == Some(id))
            .map_or(3.0, |object| object.frame.gap_mm);
        elements[index].as_object_mut().unwrap().frame.gap_mm = gap;
    }
}

/// Freeze formerly inactive head spacing before an explicit structural edit.
/// Sequence IDs must already be materialized; persist v13 after this call.
pub fn prepare_spacing(elements: &mut [LabelElement], version: u32) {
    if version < 13 {
        initialize_head_spacing(elements);
    }
}

fn normalize(elements: &mut [LabelElement], anchors: &BTreeMap<u64, f64>) {
    let mut seen = BTreeMap::new();
    for o in elements.iter_mut().filter_map(LabelElement::as_object_mut) {
        let Some(id) = o.sequence else { continue };
        if seen.insert(id, ()).is_none() {
            o.frame.position = PositionMode::Absolute;
            o.frame.x_mm = anchors[&id];
        } else {
            o.frame.position = PositionMode::Relative;
            o.frame.x_mm = 0.0;
        }
    }
}

/// Move a member between sequences, or change a free object's list order.
/// All mutations happen on a candidate document so errors leave no partial edit.
/// After a successful move in a sequence, callers persist format v13 or newer:
/// a head's gap is its dormant incoming spacing, never an origin/default cache.
pub fn reorder(
    elements: &mut Vec<LabelElement>,
    from: usize,
    to: usize,
    version: u32,
) -> Result<()> {
    if from >= elements.len() || to >= elements.len() {
        return Err(RenderError::Layout("reorder index is out of range".into()));
    }
    if from == to {
        return Ok(());
    }
    validate(elements, 11)?;
    let mut candidate = elements.clone();
    materialize(&mut candidate)?;
    prepare_spacing(&mut candidate, version);
    let source = candidate[from].as_object().unwrap().sequence;
    let target = candidate[to].as_object().unwrap().sequence;
    let anchors = anchors(&candidate);
    let mut item = candidate.remove(from);
    if let (Some(source), Some(target)) = (source, target)
        && source != target
    {
        let object = item.as_object_mut().unwrap();
        object.sequence = Some(target);
    }
    candidate.insert(to, item);
    normalize(&mut candidate, &anchors);
    validate(&candidate, 11)?;
    *elements = candidate;
    Ok(())
}

/// Connect whole sequences without changing their current physical positions.
/// Reject overlapping/interleaved groups rather than silently clamping spacing.
/// Persist v13 after success, just as for `reorder`.
pub fn connect(
    elements: &mut Vec<LabelElement>,
    index: usize,
    previous: usize,
    version: u32,
) -> Result<()> {
    let mut candidate = elements.clone();
    materialize(&mut candidate)?;
    let source = members(&candidate, index)
        .ok_or_else(|| RenderError::Layout("select a sequence to connect".into()))?;
    let destination = members(&candidate, previous)
        .ok_or_else(|| RenderError::Layout("select a preceding sequence".into()))?;
    if source.first() != Some(&index)
        || destination.last() != Some(&previous)
        || previous >= index
        || source == destination
    {
        return Err(RenderError::Layout(
            "connect separate, non-interleaved sequences".into(),
        ));
    }
    let frames = crate::scene::world_frames(&candidate)?;
    let gap = frames[index].x_mm - frames[previous].x_mm - frames[previous].width_mm;
    if gap < -1e-7 {
        return Err(RenderError::Layout(
            "sequences overlap; move the sequence start before connecting".into(),
        ));
    }
    prepare_spacing(&mut candidate, version);
    let id = candidate[previous].as_object().unwrap().sequence;
    for i in source {
        candidate[i].as_object_mut().unwrap().sequence = id;
    }
    let frame = &mut candidate[index].as_object_mut().unwrap().frame;
    frame.position = PositionMode::Relative;
    frame.x_mm = 0.0;
    frame.gap_mm = gap.max(0.0);
    validate(&candidate, 13)?;
    *elements = candidate;
    Ok(())
}

/// Unlink a member without moving any other object's physical allocation.
pub fn detach(elements: &mut [LabelElement], index: usize) -> Result<()> {
    if index >= elements.len() {
        return Err(RenderError::Layout("detach index is out of range".into()));
    }
    validate(elements, 11)?;
    materialize(elements)?;
    let Some(chain) = members(elements, index) else {
        return Ok(());
    };
    let mut cursor = LayoutCursor::default();
    let positions: Vec<_> = elements
        .iter()
        .map(|e| {
            let o = e.as_object().unwrap();
            if attached(e) {
                Ok(o.frame.x_mm)
            } else {
                cursor.place(&o.frame, o.sequence)
            }
        })
        .collect::<Result<_>>()?;
    let member = chain.iter().position(|i| *i == index).unwrap();
    if let Some(&next) = chain.get(member + 1) {
        let previous = member.checked_sub(1).map(|p| chain[p]);
        let right =
            previous.map(|p| positions[p] + elements[p].as_object().unwrap().frame.width_mm);
        let frame = &mut elements[next].as_object_mut().unwrap().frame;
        if let Some(right) = right {
            frame.gap_mm = positions[next] - right;
        } else {
            frame.position = PositionMode::Absolute;
            frame.x_mm = positions[next];
        }
    }
    let object = elements[index].as_object_mut().unwrap();
    object.sequence = None;
    object.frame.position = PositionMode::Absolute;
    object.frame.x_mm = positions[index];
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        document::{LabelDocument, LabelObject},
        layout::render_document,
        text::TextRenderer,
    };

    fn fixture() -> Vec<LabelElement> {
        (0..7)
            .map(|i| {
                LabelElement::Object(LabelObject::text(
                    format!("P{i}"),
                    TextBox {
                        width_mm: 10.0,
                        height_mm: 8.0,
                        position: if [0, 3, 6].contains(&i) {
                            PositionMode::Absolute
                        } else {
                            PositionMode::Relative
                        },
                        x_mm: match i {
                            3 => 60.0,
                            6 => 120.0,
                            _ => 0.0,
                        },
                        gap_mm: if i < 3 { 3.0 } else { 2.8 },
                        ..TextBox::default()
                    },
                ))
            })
            .collect()
    }

    fn positions(elements: &[LabelElement]) -> Vec<f64> {
        let mut cursor = LayoutCursor::default();
        elements
            .iter()
            .map(|e| {
                let o = e.as_object().unwrap();
                cursor.place(&o.frame, o.sequence).unwrap()
            })
            .collect()
    }

    fn document(elements: Vec<LabelElement>) -> LabelDocument {
        LabelDocument {
            version: 11,
            tape_width_mm: 12,
            dpi: 180,
            font_name: "Test".into(),
            font_margin: 0,
            flip_h: false,
            flip_v: false,
            elements,
        }
    }

    #[test]
    fn independent_rows_can_cross_sequences_without_rebinding_or_reflow() {
        let mut elements = fixture();
        let before = positions(&elements);
        reorder(&mut elements, 6, 1, 13).unwrap();
        assert_eq!(
            positions(&elements),
            vec![
                before[0], before[6], before[1], before[2], before[3], before[4], before[5]
            ]
        );
        assert_eq!(members(&elements, 0), Some(vec![0, 2, 3]));
        assert!(elements[1].as_object().unwrap().sequence.is_none());
        // Reorder after a free item has been interleaved in this chain.
        reorder(&mut elements, 3, 0, 13).unwrap();
        assert_eq!(members(&elements, 0), Some(vec![0, 1, 3]));
        assert_eq!(positions(&elements)[2], 120.0);
    }

    #[test]
    fn cross_sequence_moves_preserve_origins_and_incoming_spacing() {
        let mut elements = fixture();
        reorder(&mut elements, 2, 3, 13).unwrap();
        assert_eq!(members(&elements, 0), Some(vec![0, 1]));
        assert_eq!(members(&elements, 2), Some(vec![2, 3, 4, 5]));
        let p = positions(&elements);
        assert_eq!(&p[..3], &[0.0, 13.0, 60.0]);
        assert!((p[3] - 73.0).abs() < 1e-9);
        assert!((p[4] - 85.8).abs() < 1e-9);
        assert_eq!(p[6], 120.0);
        let mut elements = fixture();
        reorder(&mut elements, 3, 2, 13).unwrap();
        let p = positions(&elements);
        assert_eq!(&p[..4], &[0.0, 13.0, 25.8, 38.8]);
        assert_eq!(p[4], 60.0);
        assert!((p[5] - 72.8).abs() < 1e-9);
    }

    #[test]
    fn mixed_spacing_survives_head_promotion_crossing_and_reopening() {
        let mut elements: Vec<_> = (0..5)
            .map(|i| {
                let mut object = LabelObject::text(
                    format!("PORT {i}"),
                    TextBox {
                        width_mm: 14.5,
                        height_mm: 8.0,
                        position: if i == 0 || i == 4 {
                            PositionMode::Absolute
                        } else {
                            PositionMode::Relative
                        },
                        x_mm: if i == 4 { 72.9 } else { 0.0 },
                        gap_mm: [0.0, 2.8, 3.1, 3.1, 2.8][i],
                        ..Default::default()
                    },
                );
                object.sequence = Some(if i < 4 { 1 } else { 2 });
                LabelElement::Object(object)
            })
            .collect();
        let before = positions(&elements);
        for from in 0..4 {
            reorder(
                &mut elements,
                from,
                from + 1,
                if from == 0 { 12 } else { 13 },
            )
            .unwrap();
            let mut doc = document(elements);
            doc.version = 13;
            elements = LabelDocument::from_toml_str(&doc.to_toml_string().unwrap())
                .unwrap()
                .elements;
        }
        for from in (1..=4).rev() {
            reorder(&mut elements, from, from - 1, 13).unwrap();
        }
        assert_eq!(positions(&elements), before);
        assert_eq!(elements[1].as_object().unwrap().frame.gap_mm, 2.8);
    }

    #[test]
    fn legacy_heads_initialize_once_even_with_arbitrary_inactive_values() {
        for dormant in [0.0, 3.0, 123.0] {
            for gap in [0.0, 2.8] {
                let mut elements = fixture();
                materialize(&mut elements).unwrap();
                elements[0].as_object_mut().unwrap().frame.gap_mm = dormant;
                elements[1].as_object_mut().unwrap().frame.gap_mm = gap;
                let before = positions(&elements);
                reorder(&mut elements, 0, 1, 12).unwrap();
                assert_eq!(elements[1].as_object().unwrap().frame.gap_mm, gap);
                reorder(&mut elements, 1, 0, 13).unwrap();
                assert_eq!(positions(&elements), before);
            }
        }
    }

    #[test]
    fn generated_inverse_moves_keep_zero_gaps_widths_and_geometry() {
        let mut seed = 0x6252576879_u64;
        for case in 0..96 {
            let mut elements = fixture();
            materialize(&mut elements).unwrap();
            for (i, e) in elements.iter_mut().enumerate() {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                let f = &mut e.as_object_mut().unwrap().frame;
                f.width_mm = 2.0 + (seed % 91) as f64 / 10.0;
                f.gap_mm = if (case + i) % 3 == 0 {
                    0.0
                } else {
                    (seed % 37) as f64 / 10.0
                };
            }
            let before = elements.clone();
            let geometry = positions(&before);
            for from in 0..elements.len() {
                for to in members(&elements, from).unwrap_or_else(|| vec![from]) {
                    reorder(&mut elements, from, to, 13).unwrap();
                    reorder(&mut elements, to, from, 13).unwrap();
                    assert_eq!(elements, before, "case {case}, {from} -> {to}");
                    assert_eq!(positions(&elements), geometry);
                }
            }
        }
    }

    #[test]
    fn connecting_whole_sequences_preserves_positions_and_rejects_overlap_atomically() {
        let mut elements = fixture();
        materialize(&mut elements).unwrap();
        let before = positions(&elements);
        connect(&mut elements, 3, 2, 11).unwrap();
        assert_eq!(positions(&elements), before);
        assert_eq!(members(&elements, 0), Some(vec![0, 1, 2, 3, 4, 5]));
        let mut overlapping = fixture();
        overlapping[3].as_object_mut().unwrap().frame.x_mm = 20.0;
        let before = overlapping.clone();
        assert!(connect(&mut overlapping, 3, 2, 11).is_err());
        assert_eq!(overlapping, before);
        assert!(connect(&mut elements, 99, 2, 13).is_err());
    }

    #[test]
    fn detaching_head_middle_or_tail_keeps_every_other_allocation_in_place() {
        for index in 0..7 {
            let mut elements = fixture();
            let before = positions(&elements);
            detach(&mut elements, index).unwrap();
            assert_eq!(positions(&elements), before, "detach {index}");
            assert!(elements[index].as_object().unwrap().sequence.is_none());
            validate(&elements, 11).unwrap();
            elements[index].as_object_mut().unwrap().frame.x_mm += 4.0;
            let after = positions(&elements);
            for i in 0..7 {
                assert!((after[i] - before[i] - if i == index { 4.0 } else { 0.0 }).abs() < 1e-9);
            }
        }
    }

    #[test]
    fn repeated_moves_round_trip_and_keep_complete_object_payloads() {
        for from in 0..7 {
            for to in 0..7 {
                let mut elements = fixture();
                let payloads: Vec<_> = elements
                    .iter()
                    .map(|e| e.as_object().unwrap().content.clone())
                    .collect();
                reorder(&mut elements, from, to, 13).unwrap();
                if from != to {
                    validate(&elements, 11).unwrap();
                }
                let moved = elements[to].as_object().unwrap();
                assert_eq!(moved.content, payloads[from]);
                assert_eq!(moved.frame.width_mm, 10.0);
                let doc = document(elements);
                assert_eq!(
                    LabelDocument::from_toml_str(&doc.to_toml_string().unwrap()).unwrap(),
                    doc
                );
            }
        }
        let mut elements = fixture();
        for i in 0..100 {
            reorder(&mut elements, i % 7, (i * 3 + 1) % 7, 13).unwrap();
        }
        validate(&elements, 11).unwrap();
    }

    #[test]
    fn explicit_membership_preserves_legacy_pixels_and_uses_the_same_dpi_geometry() {
        let original = document(fixture());
        let mut linked = original.clone();
        materialize(&mut linked.elements).unwrap();
        for dpi in [180, 360] {
            let mut renderer = TextRenderer::with_test_font();
            let a =
                render_document(&original, u32::from(dpi) * 76 / 180, dpi, &mut renderer).unwrap();
            let b =
                render_document(&linked, u32::from(dpi) * 76 / 180, dpi, &mut renderer).unwrap();
            assert_eq!(a.bitmap, b.bitmap);
            reorder(&mut linked.elements, 6, 1, 13).unwrap();
            let c =
                render_document(&linked, u32::from(dpi) * 76 / 180, dpi, &mut renderer).unwrap();
            let converted = crate::layout::convert_to_objects(
                &linked,
                u32::from(dpi) * 76 / 180,
                dpi,
                &mut renderer,
            )
            .unwrap();
            let flattened =
                render_document(&converted, u32::from(dpi) * 76 / 180, dpi, &mut renderer).unwrap();
            assert_eq!(c.bitmap, flattened.bitmap);
            let x = positions(&linked.elements);
            for bound in c.bounds {
                assert_eq!(
                    bound.x,
                    (x[bound.element_index] * f64::from(dpi) / 25.4).round() as u32
                );
            }
            linked = original.clone();
            materialize(&mut linked.elements).unwrap();
        }
    }

    #[test]
    fn malformed_links_and_old_reader_versions_are_rejected() {
        let mut elements = fixture();
        materialize(&mut elements).unwrap();
        assert!(validate(&elements, 10).is_err());
        let text = document(elements.clone()).to_toml_string().unwrap();
        assert!(
            LabelDocument::from_toml_str(&text.replacen("version = 11", "version = 10", 1))
                .is_err()
        );
        for id in [0, 99] {
            let mut bad = elements.clone();
            bad[1].as_object_mut().unwrap().sequence = Some(id);
            assert!(validate(&bad, 11).is_err());
        }
        let mut bad = elements.clone();
        bad[1].as_object_mut().unwrap().frame.position = PositionMode::Absolute;
        assert!(validate(&bad, 11).is_err());
        let before = elements.clone();
        assert!(reorder(&mut elements, 9, 1, 13).is_err());
        assert_eq!(elements, before);
    }
}
