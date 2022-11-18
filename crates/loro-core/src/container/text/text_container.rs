use std::sync::{Arc, Mutex};

use rle::{
    rle_tree::{tree_trait::CumulateTreeTrait, HeapMode},
    HasLength, RleTree, RleVec, Sliceable,
};
use smallvec::{smallvec, SmallVec};

use crate::{
    container::{
        list::list_op::ListOp,
        registry::{ContainerInstance, ContainerWrapper},
        Container, ContainerID, ContainerType,
    },
    context::Context,
    dag::DagUtils,
    debug_log,
    id::{Counter, ID},
    op::{Content, Op, RemoteOp, RichOp},
    span::{HasCounterSpan, HasId, HasIdSpan, IdSpan},
    value::LoroValue,
    version::IdSpanVector,
};

use super::{
    string_pool::{Alive, StringPool},
    text_content::{ListSlice, SliceRange},
    tracker::{Effect, Tracker},
};

#[derive(Clone, Debug)]
struct DagNode {
    id: IdSpan,
    deps: SmallVec<[ID; 2]>,
}

#[derive(Debug)]
pub struct TextContainer {
    id: ContainerID,
    state: RleTree<SliceRange, CumulateTreeTrait<SliceRange, 8, HeapMode>>,
    raw_str: StringPool,
    tracker: Tracker,
    head: SmallVec<[ID; 2]>,
}

impl TextContainer {
    pub(crate) fn new(id: ContainerID) -> Self {
        Self {
            id,
            raw_str: StringPool::default(),
            tracker: Tracker::new(Default::default(), 0),
            state: Default::default(),
            // TODO: should be eq to log_store frontier?
            head: Default::default(),
        }
    }

    pub fn insert<C: Context>(&mut self, ctx: &C, pos: usize, text: &str) -> Option<ID> {
        if text.is_empty() {
            return None;
        }

        if self.state.len() < pos {
            panic!("insert index out of range");
        }

        let store = ctx.log_store();
        let mut store = store.write().unwrap();
        let id = store.next_id();
        let slice = self.raw_str.alloc(text);
        self.state.insert(pos, slice.clone().into());
        let op = Op::new(
            id,
            Content::List(ListOp::Insert {
                slice: slice.into(),
                pos,
            }),
            store.get_or_create_container_idx(&self.id),
        );
        let last_id = ID::new(
            store.this_client_id,
            op.counter + op.atom_len() as Counter - 1,
        );
        store.append_local_ops(&[op]);
        self.head = smallvec![last_id];

        Some(id)
    }

    pub fn delete<C: Context>(&mut self, ctx: &C, pos: usize, len: usize) -> Option<ID> {
        if len == 0 {
            return None;
        }

        if self.state.len() < pos + len {
            panic!("deletion out of range");
        }

        let store = ctx.log_store();
        let mut store = store.write().unwrap();
        let id = store.next_id();
        let op = Op::new(
            id,
            Content::List(ListOp::new_del(pos, len)),
            store.get_or_create_container_idx(&self.id),
        );

        let last_id = ID::new(store.this_client_id, op.ctr_last());
        store.append_local_ops(&[op]);
        self.state.delete_range(Some(pos), Some(pos + len));
        self.head = smallvec![last_id];
        Some(id)
    }

    pub fn text_len(&self) -> usize {
        self.state.len()
    }

    pub fn check(&mut self) {
        self.tracker.check();
    }

    #[cfg(feature = "test_utils")]
    pub fn debug_inspect(&mut self) {
        println!(
            "Text Container {:?}, Raw String size={}, Tree=>\n",
            self.id,
            self.raw_str.len(),
        );
        self.state.debug_inspect();
    }
}

impl Container for TextContainer {
    fn id(&self) -> &ContainerID {
        &self.id
    }

    fn type_(&self) -> ContainerType {
        ContainerType::Text
    }

    // TODO: maybe we need to let this return Cow
    fn get_value(&self) -> LoroValue {
        let mut ans_str = String::new();
        for v in self.state.iter() {
            let content = v.as_ref();
            if SliceRange::is_unknown(content) {
                panic!("Unknown range when getting value");
            }

            ans_str.push_str(&self.raw_str.get_str(&content.0));
        }

        LoroValue::String(ans_str.into_boxed_str())
    }

    fn to_export(&mut self, op: &mut RemoteOp, gc: bool) {
        if gc && self.raw_str.should_update_aliveness(self.text_len()) {
            self.raw_str
                .update_aliveness(self.state.iter().map(|x| x.as_ref().0.clone()))
        }

        let mut contents: RleVec<[Content; 1]> = RleVec::new();
        for content in op.contents.iter_mut() {
            if let Some((slice, pos)) = content.as_list_mut().and_then(|x| x.as_insert_mut()) {
                match slice {
                    ListSlice::Slice(r) => {
                        if r.is_unknown() {
                            panic!("Unknown range in state");
                        }

                        let s = self.raw_str.get_str(&r.0);
                        if gc {
                            let mut start = 0;
                            let mut pos_start = *pos;
                            for span in self.raw_str.get_aliveness(&r.0) {
                                match span {
                                    Alive::True(span) => {
                                        contents.push(Content::List(ListOp::Insert {
                                            slice: ListSlice::RawStr(s[start..start + span].into()),
                                            pos: pos_start,
                                        }));
                                    }
                                    Alive::False(span) => {
                                        let v = Content::List(ListOp::Insert {
                                            slice: ListSlice::Unknown(span),
                                            pos: pos_start,
                                        });
                                        contents.push(v);
                                    }
                                }

                                start += span.atom_len();
                                pos_start += span.atom_len();
                            }
                            assert_eq!(start, r.atom_len());
                        } else {
                            contents.push(Content::List(ListOp::Insert {
                                slice: ListSlice::RawStr(s),
                                pos: *pos,
                            }));
                        }
                    }
                    this => {
                        contents.push(Content::List(ListOp::Insert {
                            slice: this.clone(),
                            pos: *pos,
                        }));
                    }
                }
            } else {
                contents.push(content.clone());
            }
        }

        op.contents = contents;
    }

    fn to_import(&mut self, op: &mut RemoteOp) {
        debug_log!("IMPORT {:#?}", &op);
        for content in op.contents.iter_mut() {
            if let Some((slice, _pos)) = content.as_list_mut().and_then(|x| x.as_insert_mut()) {
                if let Some(slice_range) = match slice {
                    ListSlice::RawStr(s) => {
                        let range = self.raw_str.alloc(s);
                        Some(range)
                    }
                    ListSlice::Unknown(_) => None,
                    ListSlice::Slice(_) => unreachable!(),
                    ListSlice::RawData(_) => unreachable!(),
                } {
                    *slice = slice_range.into();
                }
            }
        }
        debug_log!("IMPORTED {:#?}", &op);
    }

    fn update_state_directly(&mut self, op: &RichOp) {
        match &op.get_sliced().content {
            Content::List(op) => match op {
                ListOp::Insert { slice, pos } => {
                    let v = match slice {
                        ListSlice::Slice(slice) => slice.clone(),
                        ListSlice::Unknown(u) => ListSlice::unknown_range(*u),
                        _ => unreachable!(),
                    };

                    self.state.insert(*pos, v)
                }
                ListOp::Delete(span) => self
                    .state
                    .delete_range(Some(span.start() as usize), Some(span.end() as usize)),
            },
            _ => unreachable!(),
        }
    }

    fn track_retreat(&mut self, spans: &IdSpanVector) {
        debug_log!("TRACKER RETREAT {:#?}", &spans);
        self.tracker.retreat(spans);
    }

    fn track_forward(&mut self, spans: &IdSpanVector) {
        debug_log!("TRACKER FORWARD {:#?}", &spans);
        self.tracker.forward(spans);
    }

    fn tracker_checkout(&mut self, vv: &crate::VersionVector) {
        debug_log!("Tracker checkout {:?}", vv);
        if (!vv.is_empty() || self.tracker.start_vv().is_empty())
            && self.tracker.all_vv() >= vv
            && vv >= self.tracker.start_vv()
        {
            debug_log!("OLD Tracker");
            self.tracker.checkout(vv);
        } else {
            debug_log!("NEW Tracker");
            self.tracker = Tracker::new(vv.clone(), Counter::MAX / 2);
        }
    }

    fn track_apply(&mut self, rich_op: &RichOp) {
        let op = rich_op.get_sliced();
        debug_log!("TRACKER APPLY {} {:#?}", &rich_op.id_start(), &op.content);
        let id = rich_op.id_start();
        let content = op.content;
        if self.tracker.all_vv().includes_id(rich_op.id_last()) {
            self.tracker
                .forward(&id.to_span(content.atom_len()).to_id_span_vec());
            return;
        }

        if self.tracker.all_vv().includes_id(id) {
            let this_ctr = self.tracker.all_vv().get(&id.client_id).unwrap();
            let shift = this_ctr - id.counter;
            self.tracker
                .forward(&id.to_span(shift as usize).to_id_span_vec());
            self.tracker.apply(
                id.inc(shift),
                &content.slice(shift as usize, content.atom_len()),
            );
        } else {
            self.tracker.apply(id, &content)
        }
    }

    fn apply_tracked_effects_from(
        &mut self,
        from: &crate::VersionVector,
        effect_spans: &IdSpanVector,
    ) {
        self.tracker.checkout(from);
        debug_log!("BEFORE APPLY EFFECT {:?}", self.get_value());
        for effect in self.tracker.iter_effects(effect_spans) {
            debug_log!("APPLY EFFECT {:?}", &effect);
            match effect {
                Effect::Del { pos, len } => self.state.delete_range(Some(pos), Some(pos + len)),
                Effect::Ins { pos, content } => {
                    let v = match content {
                        ListSlice::Slice(slice) => slice.clone(),
                        ListSlice::Unknown(u) => ListSlice::unknown_range(u),
                        _ => unreachable!(),
                    };

                    self.state.insert(pos, v)
                }
            }
        }
        debug_log!("AFTER APPLY EFFECT {:?}", self.get_value());
    }
}

pub struct Text {
    instance: Arc<Mutex<ContainerInstance>>,
}

impl Clone for Text {
    fn clone(&self) -> Self {
        Self {
            instance: Arc::clone(&self.instance),
        }
    }
}

impl Text {
    pub fn id(&self) -> ContainerID {
        self.instance.lock().unwrap().as_text().unwrap().id.clone()
    }

    pub fn insert<C: Context>(&mut self, ctx: &C, pos: usize, text: &str) -> Option<ID> {
        self.with_container(|x| x.insert(ctx, pos, text))
    }

    pub fn delete<C: Context>(&mut self, ctx: &C, pos: usize, len: usize) -> Option<ID> {
        self.with_container(|text| text.delete(ctx, pos, len))
    }

    // TODO: can be len?
    pub fn text_len(&self) -> usize {
        self.with_container(|text| text.text_len())
    }

    pub fn get_value(&self) -> LoroValue {
        self.instance.lock().unwrap().as_text().unwrap().get_value()
    }
}

impl ContainerWrapper for Text {
    type Container = TextContainer;

    fn with_container<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Self::Container) -> R,
    {
        let mut container_instance = self.instance.lock().unwrap();
        let text = container_instance.as_text_mut().unwrap();
        f(text)
    }
}

impl From<Arc<Mutex<ContainerInstance>>> for Text {
    fn from(text: Arc<Mutex<ContainerInstance>>) -> Self {
        Text { instance: text }
    }
}
