use std::collections::VecDeque;

use parquet2::{
    encoding::hybrid_rle::HybridRleDecoder,
    page::{split_buffer, DataPage, DictPage, Page},
    read::levels::get_bit_width,
};

use crate::{array::Array, bitmap::MutableBitmap, error::Result};

pub use super::utils::Zip;
use super::utils::{DecodedState, MaybeNext};
use super::{super::Pages, utils::PageState};

/// trait describing deserialized repetition and definition levels
pub trait Nested: std::fmt::Debug + Send + Sync {
    fn inner(&mut self) -> (Vec<i64>, Option<MutableBitmap>);

    fn push(&mut self, length: i64, is_valid: bool);

    fn is_nullable(&self) -> bool;

    fn is_repeated(&self) -> bool {
        false
    }

    // Whether the Arrow container requires all items to be filled.
    fn is_required(&self) -> bool;

    /// number of rows
    fn len(&self) -> usize;

    /// number of values associated to the primitive type this nested tracks
    fn num_values(&self) -> usize;
}

#[derive(Debug, Default)]
pub struct NestedPrimitive {
    is_nullable: bool,
    length: usize,
}

impl NestedPrimitive {
    pub fn new(is_nullable: bool) -> Self {
        Self {
            is_nullable,
            length: 0,
        }
    }
}

impl Nested for NestedPrimitive {
    fn inner(&mut self) -> (Vec<i64>, Option<MutableBitmap>) {
        (Default::default(), Default::default())
    }

    fn is_nullable(&self) -> bool {
        self.is_nullable
    }

    fn is_required(&self) -> bool {
        false
    }

    fn push(&mut self, _value: i64, _is_valid: bool) {
        self.length += 1
    }

    fn len(&self) -> usize {
        self.length
    }

    fn num_values(&self) -> usize {
        self.length
    }
}

#[derive(Debug, Default)]
pub struct NestedOptional {
    pub validity: MutableBitmap,
    pub offsets: Vec<i64>,
}

impl Nested for NestedOptional {
    fn inner(&mut self) -> (Vec<i64>, Option<MutableBitmap>) {
        let offsets = std::mem::take(&mut self.offsets);
        let validity = std::mem::take(&mut self.validity);
        (offsets, Some(validity))
    }

    fn is_nullable(&self) -> bool {
        true
    }

    fn is_repeated(&self) -> bool {
        true
    }

    fn is_required(&self) -> bool {
        // it may be for FixedSizeList
        false
    }

    fn push(&mut self, value: i64, is_valid: bool) {
        self.offsets.push(value);
        self.validity.push(is_valid);
    }

    fn len(&self) -> usize {
        self.offsets.len()
    }

    fn num_values(&self) -> usize {
        self.offsets.last().copied().unwrap_or(0) as usize
    }
}

impl NestedOptional {
    pub fn with_capacity(capacity: usize) -> Self {
        let offsets = Vec::<i64>::with_capacity(capacity + 1);
        let validity = MutableBitmap::with_capacity(capacity);
        Self { validity, offsets }
    }
}

#[derive(Debug, Default)]
pub struct NestedValid {
    pub offsets: Vec<i64>,
}

impl Nested for NestedValid {
    fn inner(&mut self) -> (Vec<i64>, Option<MutableBitmap>) {
        let offsets = std::mem::take(&mut self.offsets);
        (offsets, None)
    }

    fn is_nullable(&self) -> bool {
        false
    }

    fn is_repeated(&self) -> bool {
        true
    }

    fn is_required(&self) -> bool {
        // it may be for FixedSizeList
        false
    }

    fn push(&mut self, value: i64, _is_valid: bool) {
        self.offsets.push(value);
    }

    fn len(&self) -> usize {
        self.offsets.len()
    }

    fn num_values(&self) -> usize {
        self.offsets.last().copied().unwrap_or(0) as usize
    }
}

impl NestedValid {
    pub fn with_capacity(capacity: usize) -> Self {
        let offsets = Vec::<i64>::with_capacity(capacity + 1);
        Self { offsets }
    }
}

#[derive(Debug, Default)]
pub struct NestedStructValid {
    length: usize,
}

impl NestedStructValid {
    pub fn new() -> Self {
        Self { length: 0 }
    }
}

impl Nested for NestedStructValid {
    fn inner(&mut self) -> (Vec<i64>, Option<MutableBitmap>) {
        (Default::default(), None)
    }

    fn is_nullable(&self) -> bool {
        false
    }

    fn is_required(&self) -> bool {
        true
    }

    fn push(&mut self, _value: i64, _is_valid: bool) {
        self.length += 1;
    }

    fn len(&self) -> usize {
        self.length
    }

    fn num_values(&self) -> usize {
        self.length
    }
}

#[derive(Debug, Default)]
pub struct NestedStruct {
    validity: MutableBitmap,
}

impl NestedStruct {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            validity: MutableBitmap::with_capacity(capacity),
        }
    }
}

impl Nested for NestedStruct {
    fn inner(&mut self) -> (Vec<i64>, Option<MutableBitmap>) {
        (Default::default(), Some(std::mem::take(&mut self.validity)))
    }

    fn is_nullable(&self) -> bool {
        true
    }

    fn is_required(&self) -> bool {
        true
    }

    fn push(&mut self, _value: i64, is_valid: bool) {
        self.validity.push(is_valid)
    }

    fn len(&self) -> usize {
        self.validity.len()
    }

    fn num_values(&self) -> usize {
        self.validity.len()
    }
}

/// A decoder that knows how to map `State` -> Array
pub(super) trait NestedDecoder<'a> {
    type State: PageState<'a>;
    type Dictionary;
    type DecodedState: DecodedState;

    fn build_state(
        &self,
        page: &'a DataPage,
        dict: Option<&'a Self::Dictionary>,
    ) -> Result<Self::State>;

    /// Initializes a new state
    fn with_capacity(&self, capacity: usize) -> Self::DecodedState;

    fn push_valid(&self, state: &mut Self::State, decoded: &mut Self::DecodedState);
    fn push_null(&self, decoded: &mut Self::DecodedState);

    fn deserialize_dict(&self, page: &DictPage) -> Self::Dictionary;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitNested {
    Primitive(bool),
    List(bool),
    Struct(bool),
}

fn init_nested(init: &[InitNested], capacity: usize) -> NestedState {
    let container = init
        .iter()
        .map(|init| match init {
            InitNested::Primitive(is_nullable) => {
                Box::new(NestedPrimitive::new(*is_nullable)) as Box<dyn Nested>
            }
            InitNested::List(is_nullable) => {
                if *is_nullable {
                    Box::new(NestedOptional::with_capacity(capacity)) as Box<dyn Nested>
                } else {
                    Box::new(NestedValid::with_capacity(capacity)) as Box<dyn Nested>
                }
            }
            InitNested::Struct(is_nullable) => {
                if *is_nullable {
                    Box::new(NestedStruct::with_capacity(capacity)) as Box<dyn Nested>
                } else {
                    Box::new(NestedStructValid::new()) as Box<dyn Nested>
                }
            }
        })
        .collect();
    NestedState::new(container)
}

pub struct NestedPage<'a> {
    iter: std::iter::Peekable<std::iter::Zip<HybridRleDecoder<'a>, HybridRleDecoder<'a>>>,
}

impl<'a> NestedPage<'a> {
    pub fn try_new(page: &'a DataPage) -> Result<Self> {
        let (rep_levels, def_levels, _) = split_buffer(page)?;

        let max_rep_level = page.descriptor.max_rep_level;
        let max_def_level = page.descriptor.max_def_level;

        let reps =
            HybridRleDecoder::new(rep_levels, get_bit_width(max_rep_level), page.num_values());
        let defs =
            HybridRleDecoder::new(def_levels, get_bit_width(max_def_level), page.num_values());

        let iter = reps.zip(defs).peekable();

        Ok(Self { iter })
    }

    // number of values (!= number of rows)
    pub fn len(&self) -> usize {
        self.iter.size_hint().0
    }
}

#[derive(Debug)]
pub struct NestedState {
    pub nested: Vec<Box<dyn Nested>>,
}

impl NestedState {
    pub fn new(nested: Vec<Box<dyn Nested>>) -> Self {
        Self { nested }
    }

    /// The number of rows in this state
    pub fn len(&self) -> usize {
        // outermost is the number of rows
        self.nested[0].len()
    }
}

/// Extends `items` by consuming `page`, first trying to complete the last `item`
/// and extending it if more are needed
pub(super) fn extend<'a, D: NestedDecoder<'a>>(
    page: &'a DataPage,
    init: &[InitNested],
    items: &mut VecDeque<(NestedState, D::DecodedState)>,
    dict: Option<&'a D::Dictionary>,
    remaining: &mut usize,
    decoder: &D,
    chunk_size: Option<usize>,
) -> Result<()> {
    let mut values_page = decoder.build_state(page, dict)?;
    let mut page = NestedPage::try_new(page)?;

    let capacity = chunk_size.unwrap_or(0);
    // chunk_size = None, remaining = 44 => chunk_size = 44
    let chunk_size = chunk_size.unwrap_or(usize::MAX);

    let (mut nested, mut decoded) = if let Some((nested, decoded)) = items.pop_back() {
        (nested, decoded)
    } else {
        // there is no state => initialize it
        (init_nested(init, capacity), decoder.with_capacity(0))
    };
    let existing = nested.len();

    let additional = (chunk_size - existing).min(*remaining);

    // extend the current state
    extend_offsets2(
        &mut page,
        &mut values_page,
        &mut nested.nested,
        &mut decoded,
        decoder,
        additional,
    );
    *remaining -= nested.len() - existing;
    items.push_back((nested, decoded));

    while page.len() > 0 && *remaining > 0 {
        let additional = chunk_size.min(*remaining);

        let mut nested = init_nested(init, additional);
        let mut decoded = decoder.with_capacity(0);
        extend_offsets2(
            &mut page,
            &mut values_page,
            &mut nested.nested,
            &mut decoded,
            decoder,
            additional,
        );
        *remaining -= nested.len();
        items.push_back((nested, decoded));
    }
    Ok(())
}

fn extend_offsets2<'a, D: NestedDecoder<'a>>(
    page: &mut NestedPage<'a>,
    values_state: &mut D::State,
    nested: &mut [Box<dyn Nested>],
    decoded: &mut D::DecodedState,
    decoder: &D,
    additional: usize,
) {
    let mut values_count = vec![0; nested.len()];

    for (depth, nest) in nested.iter().enumerate().skip(1) {
        values_count[depth - 1] = nest.len() as i64
    }
    *values_count.last_mut().unwrap() = nested.last().unwrap().len() as i64;

    let mut cum_sum = vec![0u32; nested.len() + 1];
    for (i, nest) in nested.iter().enumerate() {
        let delta = nest.is_nullable() as u32 + nest.is_repeated() as u32;
        cum_sum[i + 1] = cum_sum[i] + delta;
    }

    let mut cum_rep = vec![0u32; nested.len() + 1];
    for (i, nest) in nested.iter().enumerate() {
        let delta = nest.is_repeated() as u32;
        cum_rep[i + 1] = cum_rep[i] + delta;
    }

    let max_depth = nested.len() - 1;

    let mut rows = 0;
    while let Some((rep, def)) = page.iter.next() {
        if rep == 0 {
            rows += 1;
        }

        let mut is_required = false;
        for (depth, nest) in nested.iter_mut().enumerate() {
            let right_level = rep <= cum_rep[depth] && def >= cum_sum[depth];
            if is_required || right_level {
                let is_valid = nest.is_nullable() && def > cum_sum[depth];
                let length = values_count[depth];
                nest.push(length, is_valid);
                if depth > 0 {
                    values_count[depth - 1] = nest.len() as i64;
                };
                if nest.is_required() && !is_valid {
                    is_required = true;
                } else {
                    is_required = false
                };

                if depth == max_depth {
                    // the leaf / primitive
                    let is_valid = (def != cum_sum[depth]) || !nest.is_nullable();
                    if right_level && is_valid {
                        decoder.push_valid(values_state, decoded);
                    } else {
                        decoder.push_null(decoded);
                    }
                }
            }
        }

        let next_rep = page.iter.peek().map(|x| x.0).unwrap_or(0);

        if next_rep == 0 && rows == additional {
            break;
        }
    }
}

#[inline]
pub(super) fn next<'a, I, D>(
    iter: &'a mut I,
    items: &mut VecDeque<(NestedState, D::DecodedState)>,
    dict: &'a mut Option<D::Dictionary>,
    remaining: &mut usize,
    init: &[InitNested],
    chunk_size: Option<usize>,
    decoder: &D,
) -> MaybeNext<Result<(NestedState, D::DecodedState)>>
where
    I: Pages,
    D: NestedDecoder<'a>,
{
    // front[a1, a2, a3, ...]back
    if items.len() > 1 {
        return MaybeNext::Some(Ok(items.pop_front().unwrap()));
    }
    if (items.len() == 1) && items.front().unwrap().0.len() == chunk_size.unwrap_or(usize::MAX) {
        return MaybeNext::Some(Ok(items.pop_front().unwrap()));
    }
    if *remaining == 0 {
        return match items.pop_front() {
            Some(decoded) => MaybeNext::Some(Ok(decoded)),
            None => MaybeNext::None,
        };
    }
    match iter.next() {
        Err(e) => MaybeNext::Some(Err(e.into())),
        Ok(None) => {
            if let Some(decoded) = items.pop_front() {
                MaybeNext::Some(Ok(decoded))
            } else {
                MaybeNext::None
            }
        }
        Ok(Some(page)) => {
            let page = match page {
                Page::Data(page) => page,
                Page::Dict(dict_page) => {
                    *dict = Some(decoder.deserialize_dict(dict_page));
                    return MaybeNext::More;
                }
            };

            // there is a new page => consume the page from the start
            let error = extend(
                page,
                init,
                items,
                dict.as_ref(),
                remaining,
                decoder,
                chunk_size,
            );
            match error {
                Ok(_) => {}
                Err(e) => return MaybeNext::Some(Err(e)),
            };

            if (items.len() == 1)
                && items.front().unwrap().0.len() < chunk_size.unwrap_or(usize::MAX)
            {
                MaybeNext::More
            } else {
                MaybeNext::Some(Ok(items.pop_front().unwrap()))
            }
        }
    }
}

pub type NestedArrayIter<'a> =
    Box<dyn Iterator<Item = Result<(NestedState, Box<dyn Array>)>> + Send + Sync + 'a>;
