use std::{ops::Range, sync::Arc};

use fxhash::FxHashMap;
use generic_btree::rle::HasLength;
use loro_common::{Counter, LoroValue, PeerID, ID};
use loro_preload::{CommonArena, EncodedRichtextState, TempArena};

use crate::{
    arena::SharedArena,
    container::{
        idx::ContainerIdx,
        richtext::{AnchorType, RichtextState as InnerState, StyleOp, TextStyleInfoFlag},
    },
    container::{list::list_op, richtext::richtext_state::RichtextStateChunk},
    delta::DeltaItem,
    event::Diff,
    op::{Op, RawOp},
    utils::bitmap::BitMap,
    InternalString,
};

use super::ContainerState;

#[derive(Debug)]
pub struct RichtextState {
    idx: ContainerIdx,
    state: InnerState,
    in_txn: bool,
    undo_stack: Vec<UndoItem>,
}

impl RichtextState {
    #[inline]
    pub fn new(idx: ContainerIdx) -> Self {
        Self {
            idx,
            state: Default::default(),
            in_txn: false,
            undo_stack: Default::default(),
        }
    }

    #[inline]
    pub fn to_string(&self) -> String {
        self.state.to_string()
    }

    #[inline(always)]
    pub(crate) fn is_empty(&self) -> bool {
        self.state.is_emtpy()
    }
}

impl Clone for RichtextState {
    fn clone(&self) -> Self {
        Self {
            idx: self.idx,
            state: self.state.clone(),
            in_txn: false,
            undo_stack: Vec::new(),
        }
    }
}

#[derive(Debug)]
enum UndoItem {
    Insert {
        index: u32,
        len: u32,
    },
    Delete {
        index: u32,
        content: RichtextStateChunk,
    },
}

impl ContainerState for RichtextState {
    fn apply_diff(&mut self, diff: &mut Diff, arena: &SharedArena) {
        let Diff::RichtextRaw(richtext) = diff else {
            unreachable!()
        };

        let mut index = 0;
        let mut style_starts: FxHashMap<Arc<StyleOp>, usize> = FxHashMap::default();
        for span in richtext.vec.iter() {
            match span {
                crate::delta::DeltaItem::Retain { len, meta } => {
                    index += len;
                }
                crate::delta::DeltaItem::Insert { value, meta } => {
                    match value {
                        RichtextStateChunk::Text { unicode_len, text } => {
                            self.state.insert_elem_at_entity_index(
                                index,
                                RichtextStateChunk::Text {
                                    unicode_len: *unicode_len,
                                    text: text.clone(),
                                },
                            );
                        }
                        RichtextStateChunk::Style { style, anchor_type } => {
                            self.state.insert_elem_at_entity_index(
                                index,
                                RichtextStateChunk::Style {
                                    style: style.clone(),
                                    anchor_type: *anchor_type,
                                },
                            );

                            if *anchor_type == AnchorType::Start {
                                style_starts.insert(style.clone(), index);
                            } else {
                                let start_pos =
                                    style_starts.get(style).expect("Style start not found");
                                // we need to + 1 because we also need to annotate the end anchor
                                self.state
                                    .annotate_style_range(*start_pos..index + 1, style.clone());
                            }
                        }
                    }
                    self.undo_stack.push(UndoItem::Insert {
                        index: index as u32,
                        len: value.rle_len() as u32,
                    });
                    index += value.rle_len();
                }
                crate::delta::DeltaItem::Delete { len, meta } => {
                    let content = self.state.drain_by_entity_index(index, *len);
                    for span in content {
                        self.undo_stack.push(UndoItem::Delete {
                            index: index as u32,
                            content: span,
                        })
                    }
                }
            }
        }
    }

    fn apply_op(&mut self, r_op: &RawOp, op: &Op, arena: &SharedArena) {
        match &op.content {
            crate::op::InnerContent::List(l) => match l {
                list_op::InnerListOp::Insert { slice, pos } => {
                    self.state.insert_at_entity_index(
                        *pos,
                        arena.slice_by_unicode(slice.0.start as usize..slice.0.end as usize),
                    );

                    if self.in_txn {
                        self.undo_stack.push(UndoItem::Insert {
                            index: *pos as u32,
                            len: slice.0.end - slice.0.start,
                        })
                    }
                }
                list_op::InnerListOp::Delete(del) => {
                    for span in self
                        .state
                        .drain_by_entity_index(del.start() as usize, rle::HasLength::atom_len(&del))
                    {
                        if self.in_txn {
                            self.undo_stack.push(UndoItem::Delete {
                                index: del.start() as u32,
                                content: span,
                            })
                        }
                    }
                }
                list_op::InnerListOp::StyleStart {
                    start,
                    end,
                    key,
                    info,
                } => {
                    self.state.mark_with_entity_index(
                        *start as usize..*end as usize,
                        Arc::new(StyleOp {
                            lamport: r_op.lamport,
                            peer: r_op.id.peer,
                            cnt: r_op.id.counter,
                            key: key.clone(),
                            info: *info,
                        }),
                    );
                }
                list_op::InnerListOp::StyleEnd => {}
            },
            _ => unreachable!(),
        }
    }

    fn to_diff(&self) -> Diff {
        let mut delta = crate::delta::Delta::new();
        for span in self.state.iter_chunk() {
            delta.vec.push(DeltaItem::Insert {
                value: span.clone(),
                meta: (),
            })
        }

        Diff::RichtextRaw(delta)
    }

    fn start_txn(&mut self) {
        self.in_txn = true;
    }

    fn abort_txn(&mut self) {
        self.in_txn = false;
        self.undo_all();
    }

    fn commit_txn(&mut self) {
        self.in_txn = false;
        self.undo_stack.clear();
    }

    // value is a list
    fn get_value(&self) -> LoroValue {
        LoroValue::String(Arc::new(self.state.to_string()))
    }
}

impl RichtextState {
    fn undo_all(&mut self) {
        while let Some(item) = self.undo_stack.pop() {
            match item {
                UndoItem::Insert { index, len } => {
                    let _ = self
                        .state
                        .drain_by_entity_index(index as usize, len as usize);
                }
                UndoItem::Delete { index, content } => {
                    match content {
                        RichtextStateChunk::Text { .. } => {}
                        RichtextStateChunk::Style { .. } => {
                            unimplemented!("should handle style annotation")
                        }
                    }

                    self.state
                        .insert_elem_at_entity_index(index as usize, content);
                }
            }
        }
    }

    #[inline(always)]
    pub fn len_utf8(&self) -> usize {
        self.state.len_utf8()
    }

    #[inline(always)]
    pub fn len_utf16(&self) -> usize {
        self.state.len_utf16()
    }

    #[inline(always)]
    pub fn len_entity(&self) -> usize {
        self.state.len_entity()
    }

    #[inline(always)]
    pub fn len_unicode(&self) -> usize {
        self.state.len_unicode()
    }

    #[inline(always)]
    pub(crate) fn get_entity_index_for_text_insert_pos(&self, pos: usize) -> usize {
        self.state.get_entity_index_for_text_insert_pos(pos)
    }

    #[inline(always)]
    pub(crate) fn get_entity_index_for_utf16_insert_pos(&self, pos: usize) -> usize {
        self.state.get_entity_index_for_utf16_insert_pos(pos)
    }

    #[inline(always)]
    pub(crate) fn get_text_entity_ranges_in_unicode_range(
        &self,
        pos: usize,
        len: usize,
    ) -> Vec<Range<usize>> {
        self.state.get_text_entity_ranges_in_unicode_range(pos, len)
    }

    #[inline(always)]
    pub(crate) fn get_text_entity_ranges_in_utf16_range(
        &self,
        pos: usize,
        len: usize,
    ) -> Vec<Range<usize>> {
        self.state.get_text_entity_ranges_in_utf16_range(pos, len)
    }

    #[inline(always)]
    pub fn get_richtext_value(&self) -> LoroValue {
        self.state.get_richtext_value()
    }

    #[inline(always)]
    pub(crate) fn get_loader(&mut self) -> RichtextStateLoader {
        RichtextStateLoader {
            state: self,
            start_anchor_pos: Default::default(),
        }
    }

    #[inline(always)]
    pub(crate) fn iter_chunk(&self) -> impl Iterator<Item = &RichtextStateChunk> {
        self.state.iter_chunk()
    }

    pub(crate) fn decode_snapshot(
        &mut self,
        EncodedRichtextState {
            len,
            text,
            styles,
            is_style_start,
        }: EncodedRichtextState,
        state_arena: &TempArena,
        common: &CommonArena,
        arena: &SharedArena,
    ) {
        let bit_len = is_style_start.len() * 8;
        let is_style_start = BitMap::from_vec(is_style_start, bit_len);
        let mut loader = self.get_loader();
        let mut is_text = true;
        let mut text_range_iter = text.iter();
        let mut style_iter = styles.iter();
        let mut style_index = 0;
        for &len in len.iter() {
            if is_text {
                for _ in 0..len {
                    let &range = text_range_iter.next().unwrap();
                    let text = arena.slice_by_unicode(range.0 as usize..range.1 as usize);
                    loader.push(RichtextStateChunk::new_text(text));
                }
            } else {
                for _ in 0..len {
                    let is_start = is_style_start.get(style_index);
                    style_index += 1;
                    let style_compact = style_iter.next().unwrap();
                    loader.push(RichtextStateChunk::new_style(
                        Arc::new(StyleOp {
                            lamport: style_compact.lamport,
                            peer: common.peer_ids[style_compact.peer_idx as usize],
                            cnt: style_compact.counter as Counter,
                            key: state_arena.keywords[style_compact.key_idx as usize].clone(),
                            info: TextStyleInfoFlag::from_u8(style_compact.style_info),
                        }),
                        if is_start {
                            AnchorType::Start
                        } else {
                            AnchorType::End
                        },
                    ))
                }
            }

            is_text = !is_text;
        }
    }

    pub(crate) fn encode_snapshot(
        &self,
        record_peer: &mut impl FnMut(PeerID) -> u32,
        record_key: &mut impl FnMut(&InternalString) -> usize,
    ) -> EncodedRichtextState {
        let mut len = Vec::new();
        let mut text_ranges = Vec::new();
        let mut styles = Vec::new();
        let mut is_style_start = BitMap::new();

        let mut is_last_style = false;
        for chunk in self.iter_chunk() {
            match chunk {
                RichtextStateChunk::Text { text, unicode_len } => {
                    if is_last_style || len.is_empty() {
                        len.push(1);
                    } else {
                        *len.last_mut().unwrap() += 1;
                    }

                    is_last_style = false;
                    text_ranges.push((text.start() as u32, text.end() as u32));
                }
                RichtextStateChunk::Style { style, anchor_type } => {
                    if !is_last_style {
                        len.push(1);
                    } else {
                        if len.is_empty() {
                            // zero text chunk to switch to style mode
                            len.push(0);
                            len.push(0);
                        }

                        *len.last_mut().unwrap() += 1;
                    }

                    is_last_style = true;
                    is_style_start.push(*anchor_type == AnchorType::Start);
                    styles.push(loro_preload::CompactStyleOp {
                        peer_idx: record_peer(style.peer),
                        key_idx: record_key(&style.key) as u32,
                        counter: style.cnt as u32,
                        lamport: style.lamport,
                        style_info: style.info.to_u8(),
                    })
                }
            }
        }

        EncodedRichtextState {
            len,
            text: text_ranges,
            styles,
            is_style_start: is_style_start.into_vec(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct RichtextStateLoader<'a> {
    state: &'a mut RichtextState,
    start_anchor_pos: FxHashMap<ID, usize>,
}

impl<'a> RichtextStateLoader<'a> {
    pub fn push(&mut self, elem: RichtextStateChunk) {
        match &elem {
            RichtextStateChunk::Style { style, anchor_type } => {
                // FIXME: detect style bound
            }
            RichtextStateChunk::Text { .. } => {}
        }

        self.state.state.push(elem);
    }
}