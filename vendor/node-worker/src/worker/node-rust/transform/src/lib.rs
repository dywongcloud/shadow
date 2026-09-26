use std::{cmp::min, fmt::Display, marker::PhantomData};

use oxc::{
    allocator::{Allocator, Vec},
    span::Span,
};
use smallvec::SmallVec;
use thiserror::Error;

pub mod transform;
use transform::{Transform, TransformType};

#[derive(Debug, Error)]
pub enum TransformError {
    #[cfg(not(feature = "debug"))]
    #[error("out of bounds while applying range {0}..{1} for {2} (span {3}..{4})")]
    Oob(u32, u32, &'static str, u32, u32),
    #[cfg(feature = "debug")]
    #[error(
        "out of bounds while applying range {0}..{1} for {2} (span {3}..{4}, last few spans: {5})"
    )]
    Oob(u32, u32, &'static str, u32, u32, LastSpans),

    #[cfg(feature = "debug")]
    #[error("Spans inside each other, all spans: {0}")]
    InvalidSpans(LastSpans),

    #[error("out of bounds while applying layout piece range {0}..{1} for {2}")]
    OobLayout(u32, u32, &'static str),
    #[cfg(feature = "debug")]
    #[error("Spans inside each other, all spans: {0}")]
    InvalidLayoutSpans(LastLayoutSpans),
    #[error("exactly one remainder span required")]
    InvalidRemainderSpan,
    #[error("span went across layout pieces")]
    SpanAcrossLayoutPiece,

    #[error("too much code added while applying changes at cursor {0}")]
    AddedTooLarge(u32),
    #[error("Allocator already set")]
    AllocSet,
    #[error("Allocator not set")]
    AllocUnset,
}

#[cfg(feature = "debug")]
#[derive(Debug)]
pub struct LastLayoutSpans(std::vec::Vec<Span>);
#[cfg(feature = "debug")]
impl Display for LastLayoutSpans {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "\n(current) ")?;
        for span in &self.0 {
            writeln!(f, "{}..{}", span.start, span.end)?;
        }

        Ok(())
    }
}

#[cfg(feature = "debug")]
#[derive(Debug)]
pub struct LastSpans(std::vec::Vec<(Span, std::string::String)>);
#[cfg(feature = "debug")]
impl Display for LastSpans {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "\n(current) ")?;
        for span in &self.0 {
            writeln!(f, "{}..{}: {}", span.0.start, span.0.end, span.1)?;
        }

        Ok(())
    }
}

pub enum LayoutPiece<'alloc> {
    Template(&'alloc str),
    Move(Span),
    Remainder,
}
impl LayoutPiece<'_> {
    fn get_move(&self) -> Option<&Span> {
        match self {
            Self::Move(x) => Some(x),
            _ => None,
        }
    }
}

struct RopeMap {
    original_start: u32,
    original_end: u32,
    mapped_start: u32,
}
impl RopeMap {
    pub fn new(original_start: u32, original_end: u32, mapped_start: u32) -> Self {
        Self {
            original_start,
            original_end,
            mapped_start,
        }
    }
}

struct RopePiece<'data> {
    start: u32,
    len: u32,
    str: &'data str,
}
impl<'data> RopePiece<'data> {
    pub fn new(start: u32, str: &'data str) -> Self {
        Self {
            start,
            len: str.len() as u32,
            str,
        }
    }
}

struct Layout<'alloc: 'data, 'data> {
    rope: Vec<'alloc, RopePiece<'data>>,
    map: Vec<'alloc, RopeMap>,
    len: u32,
}
impl<'alloc: 'data, 'data> Layout<'alloc, 'data> {
    pub fn new(
        alloc: &'alloc Allocator,
        str: &'data str,
        pieces: &[LayoutPiece<'alloc>],
    ) -> Result<Self, TransformError> {
        if pieces
            .iter()
            .map(|x| usize::from(matches!(x, LayoutPiece::Remainder)))
            .sum::<usize>()
            != 1
        {
            return Err(TransformError::InvalidRemainderSpan);
        }

        let mut move_spans = Vec::from_iter_in(
            pieces.iter().filter_map(LayoutPiece::get_move).copied(),
            &alloc,
        );
        let mut rope = Vec::with_capacity_in(pieces.len().saturating_add(move_spans.len()), &alloc);
        let mut map = Vec::with_capacity_in(move_spans.len().saturating_mul(2).saturating_add(1), &alloc);
        move_spans.sort_by_key(|x| x.start);

        #[cfg(feature = "debug")]
        {
            let mut last_end = 0;
            for span in &move_spans {
                if last_end > span.start {
                    let vec = move_spans.drain(..).collect();
                    return Err(TransformError::InvalidLayoutSpans(LastLayoutSpans(vec)));
                }
                last_end = span.end;
            }
        }

        macro_rules! tryget {
            ($reason:literal, $start:ident..$end:ident) => {{
                str.get($start as usize..$end as usize)
                    .ok_or_else(|| TransformError::OobLayout($start, $end, $reason))?
            }};
        }

        let mut cursor = 0;
        for piece in pieces {
            match *piece {
                LayoutPiece::Template(str) => {
                    rope.push(RopePiece::new(cursor, str));
                    cursor += str.len() as u32;
                }
                LayoutPiece::Move(Span { start, end, .. }) => {
                    map.push(RopeMap::new(start, end, cursor));
                    rope.push(RopePiece::new(cursor, tryget!("move span", start..end)));
                    cursor += end - start;
                }
                LayoutPiece::Remainder => {
                    let mut r_cursor = 0;
                    let len = str.len() as u32;

                    for Span { start, end, .. } in move_spans.iter().copied() {
                        if start > r_cursor {
                            map.push(RopeMap::new(r_cursor, start, cursor));
                            rope.push(RopePiece::new(
                                cursor,
                                tryget!("remainder cursor..start", r_cursor..start),
                            ));
                            cursor += start - r_cursor;
                        }
                        r_cursor = end;
                    }

                    if r_cursor < len {
                        map.push(RopeMap::new(r_cursor, len, cursor));
                        rope.push(RopePiece::new(
                            cursor,
                            tryget!(
                                "remainder remainder of string cursor..str.len()",
                                r_cursor..len
                            ),
                        ));
                        cursor += len - r_cursor;
                    }
                }
            }
        }

        map.sort_by_key(|x| x.original_start);

        Ok(Self {
            rope,
            map,
            len: cursor,
        })
    }

    pub fn get(&self, mut start: u32, end: u32) -> Option<SmallVec<[&'data str; 4]>> {
        if start > end || end > self.len {
            return None;
        }

        let mut vec = SmallVec::new();

        let mut rope_cursor = self.rope.partition_point(|x| x.start + x.len <= start);
        while start < end {
            let piece = self.rope.get(rope_cursor)?;
            let range = (start - piece.start) as usize..min(piece.len, end - piece.start) as usize;
            vec.push(piece.str.get(range.clone())?);
            start += range.len() as u32;
            rope_cursor += 1;
        }

        Some(vec)
    }

    pub fn transform_span(&self, span: Span) -> Option<Span> {
        let map = self
            .map
            .get(self.map.partition_point(|x| x.original_end <= span.start))?;

        if span.start < map.original_start || span.end > map.original_end {
            return None;
        }

        let start = map.mapped_start + (span.start - map.original_start);

        Some(Span::new(start, start + span.size()))
    }
}

pub struct Transformer<'alloc, 'data, T: Transform<'data>> {
    phantom: PhantomData<&'data str>,
    alloc: Option<&'alloc Allocator>,
    inner: std::vec::Vec<T>,
}

impl<'data, T: Transform<'data>> Default for Transformer<'_, 'data, T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'alloc, 'data, T: Transform<'data>> Transformer<'alloc, 'data, T> {
    pub fn new() -> Self {
        Self {
            phantom: PhantomData,
            inner: std::vec::Vec::new(),
            alloc: None,
        }
    }

    pub fn add(&mut self, rewrite: impl IntoIterator<Item = T>) {
        self.inner.extend(rewrite);
    }

    pub fn set_alloc(&mut self, alloc: &'alloc Allocator) -> Result<(), TransformError> {
        if self.alloc.is_some() {
            Err(TransformError::AllocSet)
        } else {
            self.alloc.replace(alloc);
            Ok(())
        }
    }

    pub fn take_alloc(&mut self) -> Result<(), TransformError> {
        self.alloc
            .take()
            .ok_or(TransformError::AllocUnset)
            .map(|_| ())
    }

    pub fn get_alloc(&self) -> Result<&'alloc Allocator, TransformError> {
        self.alloc.ok_or(TransformError::AllocUnset)
    }

    pub fn empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn perform(
        &mut self,
        js: &'data str,
        layout: &[LayoutPiece<'alloc>],
        data: &T::ToLowLevelData,
    ) -> Result<Vec<'alloc, u8>, TransformError> {
        let mut itoa = itoa::Buffer::new();

        let alloc = self.get_alloc()?;

        let mut cursor = 0;
        let mut offset = 0i32;
        let layout = Layout::new(alloc, js, layout)?;
        let mut buffer = Vec::with_capacity_in(layout.len as usize * 2, &alloc);

        #[cfg(feature = "debug")]
        let mut debug_vec = std::vec::Vec::new();

        macro_rules! tryget {
            ($reason:literal, $start:ident..$end:ident, $span:ident) => {{
                let ret = layout.get($start, $end);
                #[cfg(not(feature = "debug"))]
                {
                    ret.ok_or_else(|| {
                        TransformError::Oob($start, $end, $reason, $span.start, $span.end)
                    })?
                }
                #[cfg(feature = "debug")]
                {
                    ret.ok_or_else(|| {
                        TransformError::Oob(
                            $start,
                            $end,
                            $reason,
                            $span.start,
                            $span.end,
                            LastSpans(debug_vec.iter().rev().cloned().take(6).collect()),
                        )
                    })?
                }
            }};
        }

        for transform in &mut self.inner {
            transform.set_span(layout.transform_span(transform.span()).ok_or(TransformError::SpanAcrossLayoutPiece)?);
        }
        self.inner.sort();

        #[cfg(feature = "debug")]
        {
            let mut last_end = 0;
            for change in &self.inner {
                let span = change.span();
                if last_end > span.start {
                    let vec = self
                        .inner
                        .drain(..)
                        .map(|x| {
                            (
                                x.span(),
                                x.into_low_level(data, 0).to_string(&mut itoa, alloc),
                            )
                        })
                        .collect();
                    return Err(TransformError::InvalidSpans(LastSpans(vec)));
                }
                last_end = span.end;
            }
        }

        for change in self.inner.drain(..) {
            let span = change.span();
            let Span { start, end, .. } = span;

            let transform = change.into_low_level(data, offset);
            #[cfg(feature = "debug")]
            debug_vec.push((span, transform.to_string(&mut itoa, alloc)));

            for span in tryget!("cursor -> start", cursor..start, span) {
                buffer.extend_from_slice(span.as_bytes());
            }

            let len = transform.apply(&mut itoa, &mut buffer);

            match transform.ty {
                TransformType::Insert => {
                    for span in tryget!("insert: start -> end", start..end, span) {
                        buffer.extend_from_slice(span.as_bytes());
                    }

                    offset = offset.wrapping_add_unsigned(len);
                }
                TransformType::Replace => {
                    let len =
                        i32::try_from(len).map_err(|_| TransformError::AddedTooLarge(cursor))?;
                    let diff = len.wrapping_sub_unsigned(end - start);
                    offset = offset.wrapping_add(diff);
                }
            }

            cursor = end;
        }

        let js_len = layout.len;
        let span = Span::new(0, js_len);
        for span in tryget!("cursor -> js end", cursor..js_len, span) {
            buffer.extend_from_slice(span.as_bytes());
        }

        Ok(buffer)
    }
}
