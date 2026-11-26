//! This is my mini-implementation of tracks.
//! Note that a track point in this is NOT an event, but a state.
//! This means that loading from a file requires a bit of load time
//! to convert from beat saber/mod events to track points,
//! but this makes editing and playback much easier.

pub mod points;

use crate::RuntimeData;
use glam::FloatExt;
use std::borrow::Cow;
use std::cell::OnceCell;
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::ops::{Bound, Range, RangeBounds};
use tiny_vec::TinyVec;

// Errors
#[derive(thiserror::Error, Debug)]
#[error("insertion beat {got} is out of bounds 0..={bound}")]
pub struct TrackInsertionError {
    pub bound: f32,
    pub got: f32,
}

#[derive(thiserror::Error, Debug)]
pub enum TrackSetInsertionError {
    #[error(transparent)]
    OutOfBounds(#[from] TrackInsertionError),
    #[error("invalid object id: {id} expected 0..{max}")]
    InvalidId { max: usize, id: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
pub enum ShiftError {
    #[error("f32 is not finite")]
    NotFinite,
    /// The shift would push elements outside the allowed bounds.
    /// `min..=max` is the range of deltas that would have been accepted.
    #[error("shift is out of bounds {min}..={max}")]
    OutOfBounds { min: f32, max: f32 },
}


// Helpers
/// Handles base interpolation animation between points.
pub(crate) fn animate_timed<T: Timeable<Data: Point>>(
    default: &T,
    data: &[T],
    last_beat: f32,
    now: Now,
    rt: &RuntimeData,
) -> <T::Data as TimedData>::State {
    let i = data.partition_point(|p| p.beat() <= now.beat);

    let a = match i.checked_sub(1) {
        Some(j) => &data[j],
        None => default,
    };

    let extended;
    let b = match data.get(i) {
        Some(p) => p,
        None => {
            extended = T::new(a.data().extend(a.beat(), last_beat, rt), last_beat);
            &extended
        }
    };

    let span = TimeSpan::new(now, a.beat(), b.beat());

    a.data().interpolate(b.data(), span, rt)
}

/// invalidates `i`+1. `i` is left alone since it's assumed to get invalidated by
/// whatever operation caused this to be called in the first place.
fn invalidate_neighbors<T: Timeable>(list: &mut [T], i: usize) {
    if let Some(p) = list.get_mut(i.saturating_add(1)) {
        p.invalidate_cache();
    }
}

/// Clips any Data outside the song range and logs warnings.
fn clip_vec<T: Timeable>(data: &mut Vec<T>, last_beat: f32) {
    #[cfg(feature = "tracing")]
    let l = data.len();

    data.retain(|t| (0f32..=last_beat).contains(&t.beat()));

    #[cfg(feature = "tracing")]
    if l != data.len() {
        let diff = l - data.len();
        let s = if diff == 1 { "" } else { "s" };
        tracing::warn!("truncated {diff} element{s} outside the song bounds");
    }
}

/// Handles insertion logic for both Track and TrackSet
fn track_insert<T: Timeable>(
    list: &mut Vec<T>,
    last_beat: f32,
    beat: f32,
    data: T::Data,
) -> Result<usize, TrackInsertionError> {
    if (0f32..=last_beat).contains(&beat) {
        let i = list.partition_point(|p| p.beat() <= beat);

        list.insert(i, T::new(data, beat));
        invalidate_neighbors(list.as_mut_slice(), i);

        Ok(i)
    } else {
        Err(TrackInsertionError {
            bound: last_beat,
            got: beat,
        })
    }
}

/// Resolves any RangeBounds into a concrete `start..end` within `len`.
fn resolve_range(r: impl RangeBounds<usize>, len: usize) -> Option<Range<usize>> {
    let start = match r.start_bound() {
        Bound::Included(&s) => s,
        Bound::Excluded(&s) => s.checked_add(1)?,
        Bound::Unbounded => 0,
    };
    let end = match r.end_bound() {
        Bound::Included(&e) => e.checked_add(1)?,
        Bound::Excluded(&e) => e,
        Bound::Unbounded => len,
    };
    (start <= end && end <= len).then_some(start..end)
}


// Traits
/// Represents an Animator over data.
pub trait Animator<T: Timeable> {
    fn animate(default: &T, track: &[T], last_beat: f32, now: Now, rt: &RuntimeData) -> <T::Data as TimedData>::State;
}

/// Represents a type that sits at a point in time.
/// this trait determines what Animator is needed, and what the output state is.
pub trait TimedData: Clone + Default {
    /// Which animator is needed to handle this data;
    /// EventAnimator is for Events.
    /// PointAnimator is for Points.
    type Animator<T: Timeable<Data = Self>>: Animator<T>;

    /// The output state of the data interpolation.
    /// i.e. F32Point's State is f32, and an Event that produces F32Points also has an f32 State.
    type State: Clone + Default;
}

/// Points are interpolated to determine the State at the given time.
/// Points can read the point directly ahead of itself to determine its output state.
pub trait Point: TimedData {
    /// interpolates between this point and the next point.
    fn interpolate(&self, next: &Self, span: TimeSpan, runtime_data: &RuntimeData) -> Self::State;

    /// When animating at a time after the last point in the list, this is called on the last point
    /// to determine the stand-in point.
    /// By default, Clone is used, though this may not be desired in all cases.
    fn extend(&self, beat: f32, new_beat: f32, runtime_data: &RuntimeData) -> Self {
        let _ = (beat, new_beat, runtime_data);
        self.clone()
    }
}

/// Events are queried for Points that are then used for interpolation.
/// during animation, up to 3 events are sampled, and up to 2 are queried for Points.
/// Given the Events:
/// ```ignore
/// [A     B   | C]
///        ... | ...
/// ```
/// where `|` is the current time,
/// `B` is asked for point values, and given `A` as reference,
/// `C` is then asked for more point values, given `B` as reference.
/// Any points given by `B` that are placed after the first point given by `C` are discarded after
/// all of `B`'s points are handed to `Event::clip`, where interrupted interpolations can
/// have a new endpoint inserted at the cutoff time.
pub trait Event: TimedData {
    /// The Point type that this Event produces for interpolations.
    type Point: Point + TimedData<State = Self::State>;

    /// Return all points that this event turns into.
    /// Track<E: Event> handles trimming so that points from
    /// this event don't appear after the next event's points
    /// the returned points are assumed to be sorted, and is not checked.
    fn get_points<'e, T: Timeable<Data = Self>>(
        &'e self,
        event_beat: f32,
        previous: &T,
        rt: &RuntimeData,
    ) -> TinyVec<(f32, Cow<'e, Self::Point>), 4>;

    /// When the points from this event extend past the start of the next event's points,
    /// this function is intended to handle any changes needed (such as interpolating a new
    /// endpoint at or before the cutoff beat).
    /// By default, this does nothing
    fn clip<'e>(
        points: TinyVec<(f32, Cow<'e, Self::Point>), 4>,
        cutoff_beat: f32,
        rt: &RuntimeData,
    ) -> TinyVec<(f32, Cow<'e, Self::Point>), 4> {
        let _ = (cutoff_beat, rt);
        points
    }
}

pub(crate) trait PrivateTimeable<D: TimedData> {
    /// new is private.
    fn new(data: D, beat: f32) -> Self;
    /// take_data is private.
    fn take_data(self) -> D;
    fn beat_mut(&mut self) -> &mut f32;
}

/// Represents both Timed<D> and CacheableTimed<D>
#[allow(private_bounds)]
pub trait Timeable: PrivateTimeable<Self::Data> {
    type Data: TimedData;
    fn beat(&self) -> f32;
    fn data(&self) -> &<Self as Timeable>::Data;
    fn data_mut(&mut self) -> &mut <Self as Timeable>::Data;

    /// for Events, this is used to get a Point list to be used for interpolations
    fn get_points<'e>(
        &'e self,
        previous: &Self,
        rt: &RuntimeData,
    ) -> TinyVec<(f32, Cow<'e, <<Self as Timeable>::Data as Event>::Point>), 4>
    where
        <Self as Timeable>::Data: Event;

    /// for CacheableTimed<D>, this clears the event's cache,
    /// for Timed<D>, it's a no-op
    fn invalidate_cache(&mut self) {}
}

/// Represents a type where changing one element's data requires the next element to
/// recompute its own internal state.
pub trait CascadingUpdate: TimedData {
    type CascadeReferenceData: ?Sized;
    fn cascade(&mut self, beat: f32, previous: &Self, previous_beat: f32, data: &Self::CascadeReferenceData);
}

/// Indicates the cascading behavior of a Track/TrackSet.
/// The Timed and CacheableTimed types implement this
/// so that you can provide them directly without needing
/// a dummy wrapper.
pub trait Cascader {
    type Timeable: Timeable;
    type MutRef<'c>
    where
        Self::Timeable: 'c;
    type CascadeReferenceData: ?Sized;
    fn get_mut<'c>(list: &'c mut [Self::Timeable], index: usize, data: &'c Self::CascadeReferenceData) -> Option<Self::MutRef<'c>>;
    fn cascade(list: &mut [Self::Timeable], index: usize, data: &Self::CascadeReferenceData);
}


// Structs
/// Represents the span between 2 points and the current time.
#[derive(Copy, Clone, Debug)]
pub struct TimeSpan {
    pub start_beat: f32,
    pub end_beat: f32,
    pub now_beat: f32,
    start_seconds: Option<f32>,
    end_seconds: Option<f32>,
    pub now_seconds: f32,
}

/// Holds the beat and second representation of the current time.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Now {
    pub beat: f32,
    pub second: f32,
}

/// Animator for points.
/// Directly interpolates between 2 points to determine the output state.
pub struct PointAnimator<P: Point>(PhantomData<P>);

/// Animator for events.
/// First queries points from 2 target events, and then defers to PointAnimator.
pub struct EventAnimator<E: Event>(PhantomData<E>);

/// Represents a piece of data at a given time,
/// but doesn't guarantee that a Vec<UnorderedTimed<D>>
/// is always in chronological order.
pub struct UnorderedTimed<D: TimedData> {
    pub data: D,
    pub beat: f32,
}

/// Represents a piece of data at a given time.
/// Usable with both Points and Events.
pub struct Timed<D: TimedData> {
    pub data: D,
    beat: f32,
}

/// Represents a piece of data at a given time.
/// Only usable with Events, and caches the points from get_points
/// to avoid repeated calculations.
pub struct CacheableTimed<E: Event> {
    data: E,
    beat: f32,
    // Box requires an allocation, but only requires it once, and gives the benefit
    // of a smaller struct and faster reading when not looking up cache.
    // Whether this is actually more useful is entirely dependent on the size of E::Point.
    cache: OnceCell<Box<[(f32, E::Point)]>>,
}

/// Marker type that indicates that a Track/TrackSet
/// will need to do cascading updates whenever data
/// is modified.
pub struct Cascade<T: Timeable>(PhantomData<fn() -> T>);

pub struct CascadeMutGuard<'c, T: 'c + Timeable<Data: CascadingUpdate>> {
    track: &'c mut [T],
    index: usize,
    dirty: bool,
    data: &'c <T::Data as CascadingUpdate>::CascadeReferenceData,
}

/// A mutable view over part of a track that keeps beat ordering intact.
/// Only the root view (created by `slice_mut`) cascades, on drop and from index 0.
/// Sub-slices made with `slice` don't cascade, since the root outlives them.
pub struct TimedSliceMut<'d, C: Cascader> {
    /// The entire list. The active elements are `full[range]`.
    full: &'d mut [C::Timeable],
    /// Absolute indices into `full`.
    range: Range<usize>,
    /// Whether this view cascades when dropped.
    root: bool,
    /// Minimum and maximum beat that elements of the active range may have.
    bounds: (f32, f32),
    cascade_data: &'d C::CascadeReferenceData,
}

/// Holds a single animation track.
///
/// Points are looked up by a binary search which is the fastest and simplest
/// way to statelessly animate.
///
/// A track can use either Timed<D> or CacheableTimed<D> for Events,
/// and only Timed<D> for Points.
///
/// CacheableTimed<D> is only recommended if the Event you're using has a get_points implementation
/// that requires a lot of calculations or allocates memory.
///
/// `C` can be any of:
/// `Cascade<Timed<T>>` which cascades, and doesn't cache.
/// `Cascade<CacheableTimed<T>>` which cascades and caches.
/// `Timed<T>` which doesn't cascade or cache.
/// `CacheableTimed<T>` which caches and doesn't cascade.
///
/// cascading means that when a data point is modified,
/// it and all following data points are updated iteratively.
///
/// cacheable (for events only) means an event's points will
/// only recompute when the event is modified.
pub struct Track<C: Cascader> {
    data: Vec<C::Timeable>,
    /// Holds a cached default element for interpolating when the data list
    /// is empty or is animating before its first element
    default: C::Timeable,
    // only needed internally, the user should already
    // have this value anyways since it comes from them
    last_beat: f32,
}

/// Holds a set of animation tracks.
///
/// See Track<C> for more detail.
pub struct TrackSet<C: Cascader> {
    tracks: Vec<Vec<C::Timeable>>,
    /// Holds a cached default element for interpolating when the data list
    /// is empty or is animating before its first element
    default: C::Timeable,
    // only needed internally, the user should already
    // have this value anyways since it comes from them
    last_beat: f32,
}

/// Track builder for adding data elements quickly,
/// and sorting at finalization time.
pub struct UninitializedTrack<C: Cascader> {
    data: Vec<C::Timeable>,
    last_beat: f32,
}

/// TrackSet builder for adding data elements quickly,
/// and sorting at finalization time.
pub struct UninitializedTrackSet<C: Cascader> {
    tracks: Vec<Vec<C::Timeable>>,
    // only needed internally, the user should already
    // have this value anyways since it comes from them
    last_beat: f32,
}


// Implementations
impl TimeSpan {
    pub fn new(now: Now, start: f32, end: f32) -> Self {
        Self {
            start_beat: start,
            end_beat: end,
            now_beat: now.beat,
            start_seconds: None,
            end_seconds: None,
            now_seconds: now.second,
        }
    }

    pub fn get_start_seconds(&mut self, rt: &RuntimeData) -> f32 {
        *self
            .start_seconds
            .get_or_insert_with(|| rt.beat_to_seconds(self.start_beat))
    }

    pub fn get_end_seconds(&mut self, rt: &RuntimeData) -> f32 {
        *self
            .end_seconds
            .get_or_insert_with(|| rt.beat_to_seconds(self.end_beat))
    }

    pub fn inverse_lerp_beats(&self) -> f32 {
        if self.end_beat <= self.start_beat {
            return 1.0;
        }
        f32::inverse_lerp(self.start_beat, self.end_beat, self.now_beat).clamp(0., 1.)
    }

    pub fn inverse_lerp_seconds(&mut self, rt: &RuntimeData) -> f32 {
        let s = self.get_start_seconds(rt);
        let e = self.get_end_seconds(rt);
        if e <= s {
            return 1.0;
        }
        f32::inverse_lerp(s, e, self.now_seconds).clamp(0., 1.)
    }

    pub fn elapsed_beats(&self) -> f32 {
        self.now_beat - self.start_beat
    }

    pub fn elapsed_seconds(&mut self, rt: &RuntimeData) -> f32 {
        self.now_seconds - self.get_start_seconds(rt)
    }
}

impl<'d, P: Point> TimedData for Cow<'d, P> {
    type Animator<T: Timeable<Data = Cow<'d, P>>> = PointAnimator<Self>;
    type State = P::State;
}

impl<'d, P: Point> Point for Cow<'d, P> {
    fn interpolate(&self, next: &Self, span: TimeSpan, rt: &RuntimeData) -> Self::State {
        P::interpolate(self, next, span, rt)
    }
    fn extend(&self, beat: f32, new_beat: f32, rt: &RuntimeData) -> Self {
        Cow::Owned(P::extend(self, beat, new_beat, rt))
    }
}

impl<T: Timeable<Data: Point>> Animator<T> for PointAnimator<T::Data> {
    fn animate(default: &T, data: &[T], last_beat: f32, now: Now, rt: &RuntimeData) -> <T::Data as TimedData>::State {
        animate_timed(default, data, last_beat, now, rt)
    }
}

impl<T: Timeable<Data: Event>> Animator<T> for EventAnimator<T::Data> {
    fn animate(default: &T, data: &[T], last_beat: f32, now: Now, rt: &RuntimeData) -> <T::Data as TimedData>::State {
        let i = data.partition_point(|e| e.beat() <= now.beat);

        let a = match i.checked_sub(2) {
            Some(j) => &data[j],
            None => default,
        };
        let b = i.checked_sub(1).map(|j| &data[j]);
        let c = data.get(i);

        let default_point: <T::Data as Event>::Point = Default::default();
        match (b, c) {
            // Empty event list
            (None, None) => {
                let default_point = Timed::new(default_point, 0.);
                animate_timed(&default_point, &[], last_beat, now, rt)
            }
            // first or last event, `a` is default when first event,
            // and the previous event when last
            (None, Some(e)) | (Some(e), None) => {
                let default_point = Timed::new(Cow::Owned(default_point), 0.);
                let points = e.get_points(a, rt);
                let points: TinyVec<_, 4> = points
                    .into_iter()
                    .map(|(beat, data)| Timed::new(data, beat))
                    .collect();
                animate_timed(&default_point, points.as_slice(), last_beat, now, rt)
            }
            // between 2 events
            (Some(b), Some(c)) => {
                let default_point = Timed::new(Cow::Owned(default_point), 0.);

                let b_points = b.get_points(a, rt);
                debug_assert!(
                    !b_points.is_empty(),
                    "get_points returned an empty point list ({})",
                    std::any::type_name::<T::Data>()
                );
                debug_assert!(
                    b_points.windows(2).all(|w| w[0].0 <= w[1].0),
                    "get_points returned an unordered point list ({})",
                    std::any::type_name::<T::Data>()
                );

                let c_points = c.get_points(b, rt);
                debug_assert!(
                    !c_points.is_empty(),
                    "get_points returned an empty point list ({})",
                    std::any::type_name::<T::Data>()
                );
                debug_assert!(
                    c_points.windows(2).all(|w| w[0].0 <= w[1].0),
                    "get_points returned an unordered point list ({})",
                    std::any::type_name::<T::Data>()
                );

                let cutoff = c_points.first().map_or(f32::INFINITY, |(beat, _)| *beat);
                let b_points = T::Data::clip(b_points, cutoff, rt);

                let points: TinyVec<_, 8> = b_points
                    .into_iter()
                    .filter(|(beat, _)| *beat <= cutoff)
                    .chain(c_points.into_iter())
                    .map(|(beat, data)| Timed::new(data, beat))
                    .collect();

                animate_timed(&default_point, points.as_slice(), last_beat, now, rt)
            }
        }
    }
}

impl<D: TimedData> PrivateTimeable<D> for Timed<D> {
    #[inline]
    fn new(data: D, beat: f32) -> Self {
        Self { data, beat }
    }
    #[inline]
    fn take_data(self) -> D {
        self.data
    }
    #[inline]
    fn beat_mut(&mut self) -> &mut f32 {
        &mut self.beat
    }
}

impl<D: TimedData> Timeable for Timed<D> {
    type Data = D;
    #[inline]
    fn beat(&self) -> f32 {
        self.beat
    }
    #[inline]
    fn data(&self) -> &D {
        &self.data
    }
    #[inline]
    fn data_mut(&mut self) -> &mut D {
        &mut self.data
    }
    #[inline]
    fn get_points<'e>(
        &'e self,
        previous: &Self,
        rt: &RuntimeData,
    ) -> TinyVec<(f32, Cow<'e, <<Self as Timeable>::Data as Event>::Point>), 4>
    where
        <Self as Timeable>::Data: Event,
    {
        self.data.get_points(self.beat, previous, rt)
    }
}

impl<T: 'static + TimedData> Cascader for Timed<T> {
    type Timeable = Self;
    type MutRef<'c> = (f32, &'c mut T);
    type CascadeReferenceData = ();
    fn get_mut<'c>(list: &'c mut [Self], index: usize, data: &()) -> Option<Self::MutRef<'c>> {
        let _ = data;
        let t = list.get_mut(index)?;
        Some((t.beat(), t.data_mut()))
    }
    fn cascade(list: &mut [Self], index: usize, data: &()) {
        let _ = (list, index, data);
    }
}

impl<E: Event> PrivateTimeable<E> for CacheableTimed<E> {
    #[inline]
    fn new(data: E, beat: f32) -> Self {
        Self {
            data,
            beat,
            cache: Default::default(),
        }
    }
    #[inline]
    fn take_data(self) -> E {
        self.data
    }
    #[inline]
    fn beat_mut(&mut self) -> &mut f32 {
        self.invalidate_cache();
        &mut self.beat
    }
}

impl<E: Event> Timeable for CacheableTimed<E> {
    type Data = E;
    #[inline]
    fn beat(&self) -> f32 {
        self.beat
    }
    #[inline]
    fn data(&self) -> &E {
        &self.data
    }
    #[inline]
    fn data_mut(&mut self) -> &mut E {
        self.invalidate_cache();
        &mut self.data
    }

    fn get_points<'e>(
        &'e self,
        previous: &Self,
        rt: &RuntimeData,
    ) -> TinyVec<(f32, Cow<'e, <Self::Data as Event>::Point>), 4>
    where
        Self::Data: Event,
    {
        let cached = self.cache.get_or_init(|| {
            self.data
                .get_points(self.beat, previous, rt)
                .into_iter()
                .map(|(b, c)| (b, c.into_owned()))
                .collect::<Box<[_]>>()
        });
        let mut out = TinyVec::with_capacity(cached.len());
        for (b, p) in cached.iter() {
            out.push((*b, Cow::Borrowed(p)));
        }
        out
    }

    fn invalidate_cache(&mut self) {
        self.cache.take();
    }
}

impl<T: 'static + Event> Cascader for CacheableTimed<T> {
    type Timeable = Self;
    type MutRef<'c> = (f32, &'c mut T);
    type CascadeReferenceData = ();
    fn get_mut<'c>(list: &'c mut [Self], index: usize, data: &()) -> Option<Self::MutRef<'c>> {
        let _ = data;
        let t = list.get_mut(index)?;
        Some((t.beat(), t.data_mut()))
    }
    fn cascade(list: &mut [Self], index: usize, data: &()) {
        let _ = (list, index, data);
    }
}

impl<T: Timeable<Data: CascadingUpdate>> Cascader for Cascade<T> {
    type Timeable = T;
    type MutRef<'c>
        = CascadeMutGuard<'c, T>
    where
        T: 'c;
    type CascadeReferenceData = <<Self::Timeable as Timeable>::Data as CascadingUpdate>::CascadeReferenceData
    where
        <Self::Timeable as Timeable>::Data: CascadingUpdate;
    fn get_mut<'c>(list: &'c mut [T], index: usize, data: &'c Self::CascadeReferenceData) -> Option<Self::MutRef<'c>> {
        if index < list.len() {
            Some(CascadeMutGuard {
                track: list,
                index,
                dirty: false,
                data,
            })
        } else {
            None
        }
    }
    /// Recomputes every element from `data[index..]` against the
    /// previous element.
    fn cascade(list: &mut [T], index: usize, data: &Self::CascadeReferenceData) {
        for i in index..list.len() {
            let [a, b] = &mut list[i.saturating_sub(1)..=i] else {
                continue;
            };
            b.invalidate_cache();
            let b_beat = b.beat();
            b.data_mut().cascade(b_beat, a.data(), a.beat(), data);
        }
    }
}

impl<'c, T: Timeable<Data: CascadingUpdate>> CascadeMutGuard<'c, T> {
    pub fn beat(&self) -> f32 {
        self.track[self.index].beat()
    }
}

impl<'c, T: Timeable<Data: CascadingUpdate>> std::ops::Deref for CascadeMutGuard<'c, T> {
    type Target = T::Data;
    fn deref(&self) -> &T::Data {
        self.track[self.index].data()
    }
}

impl<'c, T: Timeable<Data: CascadingUpdate>> std::ops::DerefMut for CascadeMutGuard<'c, T> {
    fn deref_mut(&mut self) -> &mut T::Data {
        self.dirty = true;
        self.track[self.index].data_mut()
    }
}

impl<'c, T: 'c + Timeable<Data: CascadingUpdate>> Drop for CascadeMutGuard<'c, T> {
    fn drop(&mut self) {
        if self.dirty {
            Cascade::<T>::cascade(self.track, self.index, self.data)
        }
    }
}

impl<'d, C: Cascader> TimedSliceMut<'d, C> {
    /// `outer` is used for the side(s) of the range that touch the ends of `full`.
    fn from_parts(
        full: &'d mut [C::Timeable],
        range: Range<usize>,
        outer: (f32, f32),
        root: bool,
        cascade_data: &'d C::CascadeReferenceData,
    ) -> Self {
        let min = range.start.checked_sub(1).map_or(outer.0, |i| full[i].beat());
        let max = full.get(range.end).map_or(outer.1, |t| t.beat());
        Self { full, range, root, bounds: (min, max), cascade_data }
    }

    pub(crate) fn new_root(
        full: &'d mut [C::Timeable],
        range: Range<usize>,
        last_beat: f32,
        cascade_data: &'d C::CascadeReferenceData,
    ) -> Self {
        Self::from_parts(full, range, (0.0, last_beat), true, cascade_data)
    }

    /// Sub-slice relative to this view. Edges of the sub-slice may move up to
    /// the beats of the elements just outside it.
    pub fn slice(&mut self, r: impl RangeBounds<usize>) -> Option<TimedSliceMut<'_, C>> {
        let rel = resolve_range(r, self.range.len())?;
        let abs = self.range.start + rel.start..self.range.start + rel.end;
        Some(TimedSliceMut::from_parts(
            &mut *self.full,
            abs,
            self.bounds,
            false,
            self.cascade_data,
        ))
    }

    pub fn len(&self) -> usize {
        self.range.len()
    }

    pub fn is_empty(&self) -> bool {
        self.range.is_empty()
    }

    /// Read-only view of the active elements.
    pub fn data(&self) -> &[C::Timeable] {
        &self.full[self.range.clone()]
    }

    pub fn lower_bound(&self) -> f32 {
        self.bounds.0
    }

    pub fn upper_bound(&self) -> f32 {
        self.bounds.1
    }

    fn allowed_shift(&self) -> (f32, f32) {
        let d = self.data();
        let first = d[0].beat();
        let last = d[d.len() - 1].beat();
        (self.bounds.0 - first, self.bounds.1 - last)
    }

    fn apply_shift(&mut self, delta: f32) {
        let (lo, hi) = self.bounds;
        let range = self.range.clone();
        for t in &mut self.full[range.clone()] {
            let b = t.beat_mut();
            // Clamping guards against float drift pushing an edge out of bounds.
            // It is monotonic, so ordering is preserved.
            *b = (*b + delta).max(lo).min(hi);
            // Shifting changes this element's own cached points.
            t.invalidate_cache();
        }
        // The element after the range now has a different `previous`.
        if let Some(next) = self.full.get_mut(range.end) {
            next.invalidate_cache();
        }
    }

    /// Shifts every element by `delta`, keeping the spacing between them.
    /// If the shift were to leave the bounds, it is clamped to the largest allowed value.
    /// Errors only if `delta` is not finite.
    pub fn shift(&mut self, delta: f32) -> Result<(), ShiftError> {
        if !delta.is_finite() {
            return Err(ShiftError::NotFinite);
        }
        if self.is_empty() {
            return Ok(());
        }
        let (min, max) = self.allowed_shift();
        self.apply_shift(delta.max(min).min(max));
        Ok(())
    }

    /// Like `shift`, but errors instead of clamping.
    pub fn try_shift(&mut self, delta: f32) -> Result<(), ShiftError> {
        if !delta.is_finite() {
            return Err(ShiftError::NotFinite);
        }
        if self.is_empty() {
            return Ok(());
        }
        let (min, max) = self.allowed_shift();
        if delta < min || delta > max {
            return Err(ShiftError::OutOfBounds { min, max });
        }
        self.apply_shift(delta);
        Ok(())
    }
}

impl<'d, C: Cascader> Drop for TimedSliceMut<'d, C> {
    fn drop(&mut self) {
        if self.root {
            C::cascade(self.full, self.range.start, self.cascade_data);
        }
    }
}

impl<C: Cascader> Track<C> {
    pub fn new(last_beat: f32) -> UninitializedTrack<C> {
        UninitializedTrack::new(last_beat)
    }

    pub fn empty(last_beat: f32) -> Self {
        Self {
            data: Vec::new(),
            default: <C::Timeable>::new(Default::default(), 0.),
            last_beat,
        }
    }

    fn from_unordered_raw(
        list: Vec<UnorderedTimed<<C::Timeable as Timeable>::Data>>,
        last_beat: f32,
        data: &C::CascadeReferenceData,
    ) -> Self {
        let list = list
            .into_iter()
            .map(|u| <C::Timeable>::new(u.data, u.beat))
            .collect();
        Self::from_vec(list, last_beat, data)
    }

    fn from_vec(mut list: Vec<C::Timeable>, last_beat: f32, data: &C::CascadeReferenceData) -> Self {
        list.sort_by(|a, b| a.beat().total_cmp(&b.beat()));
        clip_vec(&mut list, last_beat);
        C::cascade(&mut list, 0, data);
        Self {
            data: list,
            default: <C::Timeable>::new(Default::default(), 0.),
            last_beat,
        }
    }

    pub fn animate(
        &self,
        now: Now,
        rt: &RuntimeData,
    ) -> <<C::Timeable as Timeable>::Data as TimedData>::State {
        <<C::Timeable as Timeable>::Data as TimedData>::Animator::animate(
            &self.default,
            &self.data,
            self.last_beat,
            now,
            rt,
        )
    }

    fn insert_raw(
        &mut self,
        beat: f32,
        data: <C::Timeable as Timeable>::Data,
        cascade_data: &C::CascadeReferenceData,
    ) -> Result<(), TrackInsertionError> {
        let i = track_insert(&mut self.data, self.last_beat, beat, data)?;
        C::cascade(&mut self.data, i, cascade_data);
        Ok(())
    }

    pub fn data(&self) -> &[C::Timeable] {
        &self.data
    }

    fn get_mut_raw<'c>(&'c mut self, index: usize, cascade_data: &'c C::CascadeReferenceData) -> Option<C::MutRef<'c>> {
        invalidate_neighbors(self.data.as_mut_slice(), index);
        C::get_mut(&mut self.data, index, cascade_data)
    }

    fn slice_mut_raw<'c>(
        &'c mut self,
        index: impl RangeBounds<usize>,
        cascade_data: &'c C::CascadeReferenceData,
    ) -> Option<TimedSliceMut<'c, C>> {
        let range = resolve_range(index, self.data.len())?;
        Some(TimedSliceMut::new_root(
            &mut self.data,
            range,
            self.last_beat,
            cascade_data,
        ))
    }

    fn remove_raw(&mut self, index: usize, cascade_data: &C::CascadeReferenceData) -> Option<(f32, <C::Timeable as Timeable>::Data)> {
        if index >= self.data.len() {
            None
        } else {
            invalidate_neighbors(self.data.as_mut_slice(), index);
            let t = self.data.remove(index);
            C::cascade(&mut self.data, index, cascade_data);
            Some((t.beat(), t.take_data()))
        }
    }

    /// Should be called after any changes are made to the RuntimeData or the Track
    pub fn invalidate_caches(&mut self) {
        self.data.iter_mut().for_each(Timeable::invalidate_cache);
    }
}

impl<C: Cascader> TrackSet<C> {
    pub fn new(tracks: usize, last_beat: f32) -> UninitializedTrackSet<C> {
        UninitializedTrackSet::new(tracks, last_beat)
    }

    pub fn empty(tracks: usize, last_beat: f32) -> Self {
        let tracks = (0..tracks).map(|_| Vec::new()).collect();
        Self {
            tracks,
            default: <C::Timeable>::new(Default::default(), 0.),
            last_beat,
        }
    }

    fn from_unordered_raw(
        list: Vec<Vec<UnorderedTimed<<C::Timeable as Timeable>::Data>>>,
        last_beat: f32,
        data: &C::CascadeReferenceData,
    ) -> Self {
        let list = list
            .into_iter()
            .map(|d| {
                d.into_iter()
                    .map(|u| <C::Timeable>::new(u.data, u.beat))
                    .collect()
            })
            .collect();
        Self::from_vecs(list, last_beat, data)
    }

    fn from_vecs(mut tracks: Vec<Vec<C::Timeable>>, last_beat: f32, data: &C::CascadeReferenceData) -> Self {
        for track in tracks.iter_mut() {
            track.sort_by(|a, b| a.beat().total_cmp(&b.beat()));
            clip_vec(track, last_beat);
            C::cascade(track, 0, data);
        }
        Self {
            tracks,
            default: <C::Timeable>::new(Default::default(), 0.),
            last_beat,
        }
    }

    pub fn animate(
        &self,
        track: usize,
        now: Now,
        rt: &RuntimeData,
    ) -> Option<<<C::Timeable as Timeable>::Data as TimedData>::State> {
        let track = self.tracks.get(track)?;
        Some(
            <<C::Timeable as Timeable>::Data as TimedData>::Animator::animate(
                &self.default,
                track,
                self.last_beat,
                now,
                rt,
            ),
        )
    }

    /// Note: if the track set has fewer tracks than the frame expects, this creates a default
    /// data value to place in the list for each missing track.
    pub fn iter_animate<R>(
        &self,
        now: Now,
        rt: &RuntimeData,
        track_count: usize,
        frame: &mut Vec<R>,
        transformer: impl Fn(<<C::Timeable as Timeable>::Data as TimedData>::State) -> R,
    ) {
        frame.reserve(track_count);
        for i in 0..track_count {
            let state = match self.tracks.get(i) {
                Some(track) => <<C::Timeable as Timeable>::Data as TimedData>::Animator::animate(
                    &self.default,
                    track,
                    self.last_beat,
                    now,
                    rt,
                ),
                None => <<C::Timeable as Timeable>::Data as TimedData>::State::default(),
            };
            frame.push(transformer(state));
        }
    }

    /// `frame` should hold uninitialized R elements.
    /// in the case that the track set has fewer
    /// tracks than the frame, it's the caller's responsibility to remove, or finish initializing
    /// the elements that this function didn't touch.
    /// this function therefore returns N as the number of values it wrote (0<=x<=frame.len())
    pub fn direct_animate<R>(
        &self,
        now: Now,
        rt: &RuntimeData,
        frame: &mut [MaybeUninit<R>],
        transformer: impl Fn(<<C::Timeable as Timeable>::Data as TimedData>::State) -> R,
    ) -> usize {
        for i in 0..frame.len() {
            if i == self.tracks.len() {
                return i;
            }
            let track = &self.tracks[i];
            let state = <<C::Timeable as Timeable>::Data as TimedData>::Animator::animate(
                &self.default,
                track,
                self.last_beat,
                now,
                rt,
            );
            frame[i].write(transformer(state));
        }
        frame.len()
    }

    pub fn track_count(&self) -> usize {
        self.tracks.len()
    }

    fn insert_raw(
        &mut self,
        track: usize,
        beat: f32,
        data: <C::Timeable as Timeable>::Data,
        cascade_data: &C::CascadeReferenceData,
    ) -> Result<(), TrackSetInsertionError> {
        if let Some(track) = self.tracks.get_mut(track) {
            let i = track_insert(track, self.last_beat, beat, data)?;
            C::cascade(track, i, cascade_data);
            Ok(())
        } else {
            Err(TrackSetInsertionError::InvalidId {
                max: self.tracks.len(),
                id: track,
            })
        }
    }

    pub fn data(&self, track: usize) -> Option<&[C::Timeable]> {
        Some(self.tracks.get(track)?)
    }

    fn get_mut_raw<'c>(&'c mut self, track: usize, index: usize, data: &'c C::CascadeReferenceData) -> Option<C::MutRef<'c>> {
        let track = self.tracks.get_mut(track)?;
        invalidate_neighbors(track.as_mut_slice(), index);
        C::get_mut(track, index, data)
    }

    fn slice_mut_raw<'c>(
        &'c mut self,
        track: usize,
        index: impl RangeBounds<usize>,
        cascade_data: &'c C::CascadeReferenceData,
    ) -> Option<TimedSliceMut<'c, C>> {
        let track = self.tracks.get_mut(track)?;
        let range = resolve_range(index, track.len())?;
        Some(TimedSliceMut::new_root(
            track.as_mut_slice(),
            range,
            self.last_beat,
            cascade_data,
        ))
    }

    fn remove_raw(
        &mut self,
        track: usize,
        index: usize,
        data: &C::CascadeReferenceData,
    ) -> Option<(f32, <C::Timeable as Timeable>::Data)> {
        let track = self.tracks.get_mut(track)?;
        if index >= track.len() {
            None
        } else {
            invalidate_neighbors(track.as_mut_slice(), index);
            let t = track.remove(index);
            C::cascade(track, index, data);
            Some((t.beat(), t.take_data()))
        }
    }

    /// Should be called after any changes are made to the RuntimeData or the TrackSet
    pub fn invalidate_caches(&mut self) {
        self.tracks.iter_mut().for_each(|t| {
            t.iter_mut().for_each(Timeable::invalidate_cache);
        });
    }
}

impl<C: Cascader> UninitializedTrack<C> {
    pub fn new(last_beat: f32) -> Self {
        Self {
            data: Vec::new(),
            last_beat,
        }
    }

    pub fn add(
        &mut self,
        beat: f32,
        data: <C::Timeable as Timeable>::Data,
    ) -> Result<(), TrackInsertionError> {
        if (0f32..=self.last_beat).contains(&beat) {
            self.data.push(<C::Timeable>::new(data, beat));
            Ok(())
        } else {
            Err(TrackInsertionError {
                bound: self.last_beat,
                got: beat,
            })
        }
    }

    fn finalize_raw(self, data: &C::CascadeReferenceData) -> Track<C> {
        Track::from_vec(self.data, self.last_beat, data)
    }
}

impl<C: Cascader> UninitializedTrackSet<C> {
    pub fn new(tracks: usize, last_beat: f32) -> Self {
        Self {
            tracks: (0..tracks).map(|_| Vec::new()).collect(),
            last_beat,
        }
    }

    pub fn add(
        &mut self,
        track: usize,
        beat: f32,
        data: <C::Timeable as Timeable>::Data,
    ) -> Result<(), TrackSetInsertionError> {
        if let Some(track) = self.tracks.get_mut(track) {
            if (0f32..=self.last_beat).contains(&beat) {
                track.push(<C::Timeable>::new(data, beat));
                Ok(())
            } else {
                Err(TrackSetInsertionError::OutOfBounds(TrackInsertionError {
                    bound: self.last_beat,
                    got: beat,
                }))
            }
        } else {
            Err(TrackSetInsertionError::InvalidId {
                max: self.tracks.len(),
                id: track,
            })
        }
    }

    fn finalize_raw(self, data: &C::CascadeReferenceData) -> TrackSet<C> {
        TrackSet::from_vecs(self.tracks, self.last_beat, data)
    }
}


// Specialized impls
macro_rules! specialized_track {
    (
        generics: [$($gen:tt)*],
        cascader: $C:ty,
        data: $Data:ty,
        extra: [$($name:ident: $ty:ty)?],
        pass: $pass:expr $(,)?
    ) => {
        impl<$($gen)*> Track<$C> {
            pub fn from_unordered(
                list: Vec<UnorderedTimed<$Data>>,
                last_beat: f32,
                $($name: &$ty,)?
            ) -> Self {
                Self::from_unordered_raw(list, last_beat, $pass)
            }

            pub fn insert(
                &mut self,
                beat: f32,
                data: $Data,
                $($name: &$ty,)?
            ) -> Result<(), TrackInsertionError> {
                self.insert_raw(beat, data, $pass)
            }

            pub fn get_mut<'c>(
                &'c mut self,
                index: usize,
                $($name: &'c $ty,)?
            ) -> Option<<$C as Cascader>::MutRef<'c>> {
                self.get_mut_raw(index, $pass)
            }

            pub fn slice_mut<'c>(
                &'c mut self,
                index: impl RangeBounds<usize>,
                $($name: &'c $ty,)?
            ) -> Option<TimedSliceMut<'c, $C>> {
                self.slice_mut_raw(index, $pass)
            }

            pub fn remove(
                &mut self,
                index: usize,
                $($name: &$ty,)?
            ) -> Option<(f32, $Data)> {
                self.remove_raw(index, $pass)
            }
        }

        impl<$($gen)*> TrackSet<$C> {
            pub fn from_unordered(
                list: Vec<Vec<UnorderedTimed<$Data>>>,
                last_beat: f32,
                $($name: &$ty,)?
            ) -> Self {
                Self::from_unordered_raw(list, last_beat, $pass)
            }

            pub fn insert(
                &mut self,
                track: usize,
                beat: f32,
                data: $Data,
                $($name: &$ty,)?
            ) -> Result<(), TrackSetInsertionError> {
                self.insert_raw(track, beat, data, $pass)
            }

            pub fn get_mut<'c>(
                &'c mut self,
                track: usize,
                index: usize,
                $($name: &'c $ty,)?
            ) -> Option<<$C as Cascader>::MutRef<'c>> {
                self.get_mut_raw(track, index, $pass)
            }

            pub fn slice_mut<'c>(
                &'c mut self,
                track: usize,
                index: impl RangeBounds<usize>,
                $($name: &'c $ty,)?
            ) -> Option<TimedSliceMut<'c, $C>> {
                self.slice_mut_raw(track, index, $pass)
            }

            pub fn remove(
                &mut self,
                track: usize,
                index: usize,
                $($name: &$ty,)?
            ) -> Option<(f32, $Data)> {
                self.remove_raw(track, index, $pass)
            }
        }

        impl<$($gen)*> UninitializedTrack<$C> {
            pub fn finalize(self $(, $name: &$ty)?) -> Track<$C> {
                self.finalize_raw($pass)
            }
        }

        impl<$($gen)*> UninitializedTrackSet<$C> {
            pub fn finalize(self $(, $name: &$ty)?) -> TrackSet<$C> {
                self.finalize_raw($pass)
            }
        }
    };
}

specialized_track! {
    generics: [T: Timeable<Data: CascadingUpdate>],
    cascader: Cascade<T>,
    data: T::Data,
    extra: [cascade_data: <T::Data as CascadingUpdate>::CascadeReferenceData],
    pass: cascade_data,
}

specialized_track! {
    generics: [D: 'static + TimedData],
    cascader: Timed<D>,
    data: D,
    extra: [],
    pass: &(),
}

specialized_track! {
    generics: [D: 'static + Event],
    cascader: CacheableTimed<D>,
    data: D,
    extra: [],
    pass: &(),
}


// Macros
/// Simplifies implementing TimedData for types.
/// Usage:
/// ```rs
/// timed!(point MyPoint: f32); // sets State = f32, and Animator = PointAnimator
/// timed!(event MyEvent: f32); // sets State = f32, and Animator = EventAnimator
/// ```
#[macro_export]
macro_rules! timed {
    (event $name:ident: $state:ty) => {
        impl $crate::tracks::TimedData for $name {
            type Animator<T: $crate::tracks::Timeable<Data = Self>> =
                $crate::tracks::EventAnimator<Self>;
            type State = $state;
        }
    };
    (point $name:ident: $state:ty) => {
        impl $crate::tracks::TimedData for $name {
            type Animator<T: $crate::tracks::Timeable<Data = Self>> =
                $crate::tracks::PointAnimator<Self>;
            type State = $state;
        }
    };
}

pub use timed;
