use std::{
    collections::{HashMap, HashSet},
    fmt,
    hash::{Hash, Hasher},
};

use anyhow::{Result, anyhow};
use log::{debug, info};
use opencv::core::{MatTraitConst, Point, Rect, Vec4b};

use crate::{
    array::Array,
    context::{Context, Contextual, ControlFlow},
    detect::{Detector, OtherPlayerKind},
    network::NotificationKind,
    pathing::{
        MAX_PLATFORMS_COUNT, Platform, PlatformWithNeighbors, find_neighbors, find_platforms_bound,
    },
    player::{DOUBLE_JUMP_THRESHOLD, GRAPPLING_MAX_THRESHOLD, JUMP_THRESHOLD, Player},
    task::{Task, Update, update_detection_task},
};

const MINIMAP_BORDER_WHITENESS_THRESHOLD: u8 = 160;
const MAX_PORTALS_COUNT: usize = 16;

/// A wrapper struct for [`Rect`] that implements [`Hash`].
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
struct HashedRect {
    inner: Rect,
}

impl Hash for HashedRect {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.inner.x.hash(state);
        self.inner.y.hash(state);
        self.inner.width.hash(state);
        self.inner.height.hash(state);
    }
}

/// Minimap persistent state.
#[derive(Debug, Default)]
pub struct MinimapState {
    /// Task to detect the current minimap bounding box and anchor points.
    minimap_task: Option<Task<Result<(Anchors, Rect)>>>,
    /// Task to detect the current minimap's rune.
    rune_task: Option<Task<Result<Point>>>,
    /// Task to detect the current minimap's portals.
    portals_task: Option<Task<Result<Vec<Rect>>>>,
    /// Map to invalidate portals.
    ///
    /// If there is any false-positive portal, this helps remove that portal over time to ensure
    /// player's action will not get wrongly cancelled (e.g. in up jump).
    portals_invalidate_map: HashMap<HashedRect, u32>,
    /// Task to detect elite boss.
    has_elite_boss_task: Option<Task<Result<()>>>,
    /// Task to detect guildie player(s) in the minimap.
    has_guildie_player_task: Option<Task<Result<()>>>,
    /// Task to detect stranger player(s) in the minimap.
    has_stranger_player_task: Option<Task<Result<()>>>,
    /// Task to detect firend player(s) in the minimap.
    has_friend_player_task: Option<Task<Result<()>>>,

    platforms: Vec<Platform>,
    /// Whether to update the [`MinimapIdle::platforms`].
    ///
    /// This is set to true each time [`Self::data`] is updated.
    platforms_dirty: bool,
}

impl MinimapState {
    pub fn set_platforms(&mut self, platforms: Vec<Platform>) {
        self.platforms = platforms;
        self.platforms_dirty = true;
    }
}

#[derive(Clone, Copy, Debug)]
#[cfg_attr(test, derive(Default, PartialEq))]
struct Anchors {
    tl: (Point, Vec4b),
    br: (Point, Vec4b),
}

#[derive(Clone, Copy, Debug)]
#[cfg_attr(test, derive(Default))]
pub struct Threshold<T> {
    value: Option<T>,
    fail_count: u32,
    max_fail_count: u32,
}

impl<T> Threshold<T> {
    fn new(max_fail_count: u32) -> Self {
        Self {
            value: None,
            fail_count: 0,
            max_fail_count,
        }
    }
}

#[derive(Clone, Copy, Debug)]
#[cfg_attr(test, derive(Default))]
pub struct MinimapIdle {
    /// Two anchors top left and bottom right of the minimap.
    ///
    /// They are just two fixed pixels used to know if the the minimap has moved or some other UI
    /// overlapping the minimap.
    anchors: Anchors,
    /// The bounding box of the minimap.
    ///
    /// This is in OpenCV native coordinate, which is top-left.
    pub bbox: Rect,
    /// Whether minimap UI is being partially overlapped.
    ///
    /// It is partially overlapped by other UIs if one of the anchor mismatches.
    pub partially_overlapping: bool,
    /// The rune position.
    ///
    /// The rune position is in player-relative coordinate, which is bottom-left.
    rune: Threshold<Point>,
    /// Whether there is an elite boss.
    ///
    /// TODO: This does not belong to minimap.
    has_elite_boss: Threshold<()>,
    /// Whether there is a guildie.
    has_guildie_player: Threshold<()>,
    /// Whether there is a stranger.
    has_stranger_player: Threshold<()>,
    /// Whether there is a friend.
    has_friend_player: Threshold<()>,
    /// The portal positions.
    ///
    /// The portals are in player-relative coordinate, which is bottom-left.
    portals: Array<Rect, MAX_PORTALS_COUNT>,
    /// The user provided platforms.
    ///
    /// The platforms are in player-relative coordinate, which is bottom-left.
    pub platforms: Array<PlatformWithNeighbors, MAX_PLATFORMS_COUNT>,
    /// The largest rectangle containing all the platforms.
    ///
    /// The platforms bound is in OpenCV native coordinate, which is top-left.
    pub platforms_bound: Option<Rect>,
}

impl MinimapIdle {
    #[inline]
    pub fn rune(&self) -> Option<Point> {
        self.rune.value
    }

    #[cfg(test)]
    pub fn set_rune(&mut self, rune: Point) {
        self.rune.value = Some(rune);
    }

    #[inline]
    pub fn portals(&self) -> Array<Rect, MAX_PORTALS_COUNT> {
        self.portals
    }

    #[inline]
    pub fn has_elite_boss(&self) -> bool {
        self.has_elite_boss.value.is_some()
    }

    #[inline]
    pub fn has_any_other_player(&self) -> bool {
        self.has_guildie_player.value.is_some()
            || self.has_stranger_player.value.is_some()
            || self.has_friend_player.value.is_some()
    }

    #[inline]
    pub fn is_position_inside_portal(&self, pos: Point) -> bool {
        for portal in self.portals {
            let x_range = portal.x..(portal.x + portal.width);
            let y_range = portal.y..(portal.y + portal.height);

            if x_range.contains(&pos.x) && y_range.contains(&pos.y) {
                info!(target: "minimap", "position {pos:?} is inside portal {portal:?}");
                return true;
            }
        }
        false
    }
}

#[derive(Clone, Copy, Debug)]
#[allow(clippy::large_enum_variant)] // There is only ever a single instance of Minimap
pub enum Minimap {
    Detecting,
    Idle(MinimapIdle),
}

impl Contextual for Minimap {
    type Persistent = MinimapState;

    fn update(self, context: &Context, state: &mut MinimapState) -> ControlFlow<Self> {
        ControlFlow::Next(update_context(self, context, state))
    }
}

#[inline]
fn update_context(contextual: Minimap, context: &Context, state: &mut MinimapState) -> Minimap {
    match contextual {
        Minimap::Detecting => update_detecting_context(context, state),
        Minimap::Idle(idle) => {
            update_idle_context(context, state, idle).unwrap_or(Minimap::Detecting)
        }
    }
}

fn update_detecting_context(context: &Context, state: &mut MinimapState) -> Minimap {
    let Update::Ok((anchors, bbox)) =
        update_detection_task(context, 2000, &mut state.minimap_task, move |detector| {
            let bbox = detector.detect_minimap(MINIMAP_BORDER_WHITENESS_THRESHOLD)?;
            let size = bbox.width.min(bbox.height) as usize;
            let tl = anchor_at(detector.mat(), bbox.tl(), size, 1)?;
            let br = anchor_at(detector.mat(), bbox.br(), size, -1)?;
            let anchors = Anchors { tl, br };
            debug!(target: "minimap", "anchor points: {anchors:?}");
            Ok((anchors, bbox))
        })
    else {
        return Minimap::Detecting;
    };

    let (platforms, platforms_bound) = platforms_and_bound(bbox, &state.platforms);
    state.platforms_dirty = false;
    state.rune_task = None;
    state.portals_task = None;
    state.portals_invalidate_map.clear();
    state.has_elite_boss_task = None;
    state.has_guildie_player_task = None;
    state.has_stranger_player_task = None;
    state.has_friend_player_task = None;

    Minimap::Idle(MinimapIdle {
        anchors,
        bbox,
        partially_overlapping: false,
        rune: Threshold::new(3),
        has_elite_boss: Threshold::new(2),
        has_guildie_player: Threshold::new(2),
        has_stranger_player: Threshold::new(2),
        has_friend_player: Threshold::new(2),
        portals: Array::new(),
        platforms,
        platforms_bound,
    })
}

fn update_idle_context(
    context: &Context,
    state: &mut MinimapState,
    idle: MinimapIdle,
) -> Option<Minimap> {
    if matches!(context.player, Player::CashShopThenExit(_, _)) {
        return Some(Minimap::Idle(idle));
    }

    let MinimapIdle {
        anchors,
        bbox,
        rune,
        has_elite_boss,
        has_guildie_player,
        has_stranger_player,
        has_friend_player,
        portals,
        mut platforms,
        mut platforms_bound,
        ..
    } = idle;
    let tl_pixel = pixel_at(context.detector_unwrap().mat(), anchors.tl.0)?;
    let br_pixel = pixel_at(context.detector_unwrap().mat(), anchors.br.0)?;
    let tl_match = anchor_match(anchors.tl.1, tl_pixel);
    let br_match = anchor_match(anchors.br.1, br_pixel);
    if !tl_match && !br_match {
        debug!(
            target: "minimap",
            "anchor pixels mismatch: {:?} != {:?}",
            (tl_pixel, br_pixel),
            (anchors.tl.1, anchors.br.1)
        );
        return None;
    }

    let partially_overlapping = (tl_match && !br_match) || (!tl_match && br_match);
    let rune = update_rune_task(context, &mut state.rune_task, bbox, rune);
    let has_elite_boss =
        update_elite_boss_task(context, &mut state.has_elite_boss_task, has_elite_boss);
    let has_guildie_player = update_other_player_task(
        context,
        &mut state.has_guildie_player_task,
        bbox,
        has_guildie_player,
        OtherPlayerKind::Guildie,
    );
    let has_stranger_player = update_other_player_task(
        context,
        &mut state.has_stranger_player_task,
        bbox,
        has_stranger_player,
        OtherPlayerKind::Stranger,
    );
    let has_friend_player = update_other_player_task(
        context,
        &mut state.has_friend_player_task,
        bbox,
        has_friend_player,
        OtherPlayerKind::Friend,
    );
    let portals = update_portals_task(
        context,
        &mut state.portals_task,
        &mut state.portals_invalidate_map,
        portals,
        bbox,
    );

    if state.platforms_dirty {
        let (updated_platforms, updated_bound) = platforms_and_bound(bbox, &state.platforms);
        platforms = updated_platforms;
        platforms_bound = updated_bound;
        state.platforms_dirty = false;
    }

    Some(Minimap::Idle(MinimapIdle {
        partially_overlapping,
        rune,
        has_elite_boss,
        has_guildie_player,
        has_stranger_player,
        has_friend_player,
        portals,
        platforms,
        platforms_bound,
        ..idle
    }))
}

#[inline]
fn anchor_match(anchor: Vec4b, pixel: Vec4b) -> bool {
    const ANCHOR_ACCEPTABLE_ERROR_RANGE: u32 = 45;

    let b = anchor[0].abs_diff(pixel[0]) as u32;
    let g = anchor[1].abs_diff(pixel[1]) as u32;
    let r = anchor[2].abs_diff(pixel[2]) as u32;
    let avg = (b + g + r) / 3; // Average for grayscale
    avg <= ANCHOR_ACCEPTABLE_ERROR_RANGE
}

#[inline]
fn update_rune_task(
    context: &Context,
    task: &mut Option<Task<Result<Point>>>,
    minimap: Rect,
    rune: Threshold<Point>,
) -> Threshold<Point> {
    let was_none = rune.value.is_none();
    if matches!(context.player, Player::SolvingRune(_)) && !was_none {
        return rune;
    }

    let rune = update_threshold_detection(context, 5000, rune, task, move |detector| {
        detector
            .detect_minimap_rune(minimap)
            .map(|rune| center_of_bbox(rune, minimap))
    });

    if was_none && rune.value.is_some() && !context.operation.halting() {
        info!(target: "minimap", "sending notification for rune...");
        let _ = context
            .notification
            .schedule_notification(NotificationKind::RuneAppear);
    }
    rune
}

#[inline]
fn update_elite_boss_task(
    context: &Context,
    task: &mut Option<Task<Result<()>>>,
    has_elite_boss: Threshold<()>,
) -> Threshold<()> {
    let did_have_elite_boss = has_elite_boss.value.is_some();
    let has_elite_boss =
        update_threshold_detection(context, 5000, has_elite_boss, task, move |detector| {
            if detector.detect_elite_boss_bar() {
                Ok(())
            } else {
                Err(anyhow!("no elite boss detected"))
            }
        });

    if !context.operation.halting() && !did_have_elite_boss && has_elite_boss.value.is_some() {
        info!(target: "minimap", "sending elite boss notification...");
        let _ = context
            .notification
            .schedule_notification(NotificationKind::EliteBossAppear);
    }
    has_elite_boss
}

#[inline]
fn update_other_player_task(
    context: &Context,
    task: &mut Option<Task<Result<()>>>,
    minimap: Rect,
    threshold: Threshold<()>,
    kind: OtherPlayerKind,
) -> Threshold<()> {
    let has_player = threshold.value.is_some();
    let threshold = update_threshold_detection(context, 3000, threshold, task, move |detector| {
        if detector.detect_player_kind(minimap, kind) {
            Ok(())
        } else {
            Err(anyhow!("player not found"))
        }
    });
    if !context.operation.halting() && !has_player && threshold.value.is_some() {
        info!(target: "minimap", "sending {kind:?} notification...");
        let notification = match kind {
            OtherPlayerKind::Guildie => NotificationKind::PlayerGuildieAppear,
            OtherPlayerKind::Stranger => NotificationKind::PlayerStrangerAppear,
            OtherPlayerKind::Friend => NotificationKind::PlayerFriendAppear,
        };
        let _ = context.notification.schedule_notification(notification);
    }
    threshold
}

#[inline]
fn update_portals_task(
    context: &Context,
    task: &mut Option<Task<Result<Vec<Rect>>>>,
    invalidate_map: &mut HashMap<HashedRect, u32>,
    portals: Array<Rect, MAX_PORTALS_COUNT>,
    minimap: Rect,
) -> Array<Rect, MAX_PORTALS_COUNT> {
    let update = update_detection_task(context, 5000, task, move |detector| {
        Ok(detector.detect_minimap_portals(minimap))
    });
    match update {
        Update::Ok(vec) => {
            let new_portals = vec
                .into_iter()
                .map(|portal| HashedRect {
                    inner: Rect::new(
                        portal.x,
                        minimap.height - portal.br().y, // Flip coordinate to bottom-left
                        portal.width,
                        portal.height,
                    ),
                })
                .collect::<HashSet<_>>();
            let old_portals = portals
                .into_iter()
                .map(|portal| HashedRect { inner: portal })
                .collect::<HashSet<_>>();

            merge_portals_and_invalidate_if_needed(old_portals, new_portals, invalidate_map)
        }
        Update::Err(_) | Update::Pending => portals,
    }
}

fn merge_portals_and_invalidate_if_needed(
    old_portals: HashSet<HashedRect>,
    new_portals: HashSet<HashedRect>,
    invalidate_map: &mut HashMap<HashedRect, u32>,
) -> Array<Rect, MAX_PORTALS_COUNT> {
    const INVALIDATE_THRESHOLD: u32 = 3;

    let mut merged_portals = new_portals
        .union(&old_portals)
        .copied()
        .collect::<HashSet<_>>();

    // Reset all the intersection portals invalidate count to 0
    for portal in new_portals.intersection(&old_portals) {
        invalidate_map
            .entry(*portal)
            .and_modify(|count| *count = 0)
            .or_insert(0);
    }
    // Increment detection failed portals invalidate count
    for portal in old_portals.difference(&new_portals) {
        let count = invalidate_map
            .entry(*portal)
            .and_modify(|count| *count += 1)
            .or_insert(1);
        if *count >= INVALIDATE_THRESHOLD {
            invalidate_map.remove(portal);
            merged_portals.remove(portal);
        }
    }
    if merged_portals.len() >= MAX_PORTALS_COUNT {
        // TODO: Truncate instead?
        invalidate_map.clear();
        merged_portals.clear();
    }

    Array::from_iter(merged_portals.into_iter().map(|portal| portal.inner))
}

fn platforms_and_bound(
    bbox: Rect,
    platforms: &[Platform],
) -> (Array<PlatformWithNeighbors, 24>, Option<Rect>) {
    let platforms = Array::from_iter(find_neighbors(
        platforms,
        DOUBLE_JUMP_THRESHOLD,
        JUMP_THRESHOLD,
        GRAPPLING_MAX_THRESHOLD,
    ));
    let bound = find_platforms_bound(bbox, &platforms);
    (platforms, bound)
}

#[inline]
fn update_threshold_detection<T, F>(
    context: &Context,
    repeat_delay_millis: u64,
    mut threshold: Threshold<T>,
    threshold_task: &mut Option<Task<Result<T>>>,
    threshold_task_fn: F,
) -> Threshold<T>
where
    T: fmt::Debug + Send + 'static,
    F: FnOnce(Box<dyn Detector>) -> Result<T> + Send + 'static,
{
    let update = update_detection_task(
        context,
        repeat_delay_millis,
        threshold_task,
        threshold_task_fn,
    );

    match update {
        Update::Ok(value) => {
            threshold.value = Some(value);
            threshold.fail_count = 0;
        }
        Update::Err(_) => {
            if threshold.value.is_some() {
                threshold.fail_count += 1;
                if threshold.fail_count >= threshold.max_fail_count {
                    threshold.value = None;
                    threshold.fail_count = 0;
                }
            }
        }
        Update::Pending => (),
    }

    threshold
}

#[inline]
fn center_of_bbox(bbox: Rect, minimap: Rect) -> Point {
    let tl = bbox.tl();
    let br = bbox.br();
    let x = (tl.x + br.x) / 2;
    let y = minimap.height - br.y + 1;
    Point::new(x, y)
}

#[inline]
fn pixel_at(mat: &impl MatTraitConst, point: Point) -> Option<Vec4b> {
    mat.at_pt::<Vec4b>(point).ok().copied()
}

#[inline]
fn anchor_at(
    mat: &impl MatTraitConst,
    offset: Point,
    size: usize,
    sign: i32,
) -> Result<(Point, Vec4b)> {
    (0..size)
        .find_map(|i| {
            let value = sign * i as i32;
            let diag = offset + Point::new(value, value);
            let pixel = pixel_at(mat, diag)?;
            if pixel
                .iter()
                .all(|v| *v >= MINIMAP_BORDER_WHITENESS_THRESHOLD)
            {
                Some((diag, pixel))
            } else {
                None
            }
        })
        .ok_or(anyhow!("anchor not found"))
}

#[cfg(test)]
mod tests {
    use std::{assert_matches::assert_matches, time::Duration};

    use mockall::predicate::eq;
    use opencv::core::{Mat, MatExprTraitConst, MatTrait, Point, Rect, Vec4b};
    use tokio::time;

    use super::*;
    use crate::detect::MockDetector;

    fn create_test_mat() -> (Mat, Anchors) {
        let mut mat = Mat::zeros(100, 100, opencv::core::CV_8UC4)
            .unwrap()
            .to_mat()
            .unwrap();
        let pixel = Vec4b::all(255);
        let tl = Point::new(10, 10);
        let br = Point::new(90, 90);
        *mat.at_pt_mut::<Vec4b>(tl).unwrap() = Vec4b::all(255);
        *mat.at_pt_mut::<Vec4b>(br).unwrap() = Vec4b::all(255);
        (
            mat,
            Anchors {
                tl: (tl, pixel),
                br: (br, pixel),
            },
        )
    }

    fn create_mock_detector() -> (MockDetector, Rect, Anchors, Rect) {
        let mut detector = MockDetector::new();
        let (mat, anchors) = create_test_mat();
        let bbox = Rect::new(0, 0, 100, 100);
        let rune_bbox = Rect::new(40, 40, 20, 20);
        detector
            .expect_detect_minimap_rune()
            .withf(move |b| *b == bbox)
            .returning(move |_| Ok(rune_bbox));
        detector
            .expect_clone()
            .returning(|| create_mock_detector().0);
        detector
            .expect_detect_minimap()
            .with(eq(MINIMAP_BORDER_WHITENESS_THRESHOLD))
            .returning(move |_| Ok(bbox));
        detector.expect_mat().return_const(mat.into());
        (detector, bbox, anchors, rune_bbox)
    }

    async fn advance_task(
        contextual: Minimap,
        detector: MockDetector,
        state: &mut MinimapState,
    ) -> Minimap {
        let context = Context::new(None, Some(detector));
        let completed = |state: &MinimapState| {
            if matches!(contextual, Minimap::Idle(_)) {
                state.rune_task.as_ref().unwrap().completed()
            } else {
                state.minimap_task.as_ref().unwrap().completed()
            }
        };
        let mut minimap = update_context(contextual, &context, state);
        while !completed(state) {
            minimap = update_context(minimap, &context, state);
            time::advance(Duration::from_millis(1000)).await;
        }
        minimap
    }

    #[tokio::test(start_paused = true)]
    async fn minimap_detecting_to_idle() {
        let mut state = MinimapState::default();
        let (detector, bbox, anchors, _) = create_mock_detector();

        let minimap = advance_task(Minimap::Detecting, detector, &mut state).await;
        assert_matches!(minimap, Minimap::Idle(_));
        match minimap {
            Minimap::Idle(idle) => {
                assert_eq!(idle.anchors, anchors);
                assert_eq!(idle.bbox, bbox);
                assert!(!idle.partially_overlapping);
                assert_eq!(idle.rune.value, None);
                assert!(!idle.has_elite_boss());
                assert!(!idle.has_any_other_player());
                assert!(idle.portals.is_empty());

                assert_matches!(state.minimap_task, Some(_));
                assert_matches!(state.rune_task, None);
                assert_matches!(state.has_elite_boss_task, None);
                assert_matches!(state.has_guildie_player_task, None);
                assert_matches!(state.has_stranger_player_task, None);
                assert_matches!(state.has_friend_player_task, None);
                assert_matches!(state.portals_task, None);
                assert!(state.portals_invalidate_map.is_empty());
            }
            _ => unreachable!(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn minimap_idle_rune_detection() {
        let mut state = MinimapState::default();
        let (detector, bbox, anchors, rune_bbox) = create_mock_detector();

        let idle = MinimapIdle {
            anchors,
            bbox,
            partially_overlapping: false,
            rune: Threshold::new(3),
            has_elite_boss: Threshold::default(),
            has_guildie_player: Threshold::default(),
            has_stranger_player: Threshold::default(),
            has_friend_player: Threshold::default(),
            portals: Array::new(),
            platforms: Array::new(),
            platforms_bound: None,
        };

        let minimap = advance_task(Minimap::Idle(idle), detector, &mut state).await;
        assert_matches!(minimap, Minimap::Idle(_));
        match minimap {
            Minimap::Idle(idle) => {
                assert_eq!(idle.rune.value, Some(center_of_bbox(rune_bbox, bbox)));
            }
            _ => unreachable!(),
        }
    }

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect::new(x, y, w, h)
    }

    fn hashed(x: i32, y: i32, w: i32, h: i32) -> HashedRect {
        HashedRect {
            inner: rect(x, y, w, h),
        }
    }

    #[test]
    fn merge_portals_and_invalidate_if_needed_normal() {
        let old = HashSet::from([hashed(0, 0, 10, 10)]);
        let new = HashSet::from([hashed(10, 10, 5, 5)]);
        let mut map = HashMap::new();

        let merged = merge_portals_and_invalidate_if_needed(old, new, &mut map)
            .into_iter()
            .collect::<Vec<_>>();
        let expected = vec![rect(0, 0, 10, 10), rect(10, 10, 5, 5)];

        assert_eq!(merged.len(), 2);
        for rect in expected {
            assert!(merged.contains(&rect));
        }
    }

    #[test]
    fn merge_portals_and_invalidate_if_needed_reset_invalidation_count_on_match() {
        let portal = hashed(1, 1, 5, 5);
        let old = HashSet::from([portal]);
        let new = HashSet::from([portal]);
        let mut map = HashMap::from([(portal, 2)]);

        merge_portals_and_invalidate_if_needed(old, new, &mut map);
        assert_eq!(map.get(&portal), Some(&0));
    }

    #[test]
    fn merge_portals_and_invalidate_if_needed_increment_invalidation_count_on_missing() {
        let portal = hashed(2, 2, 4, 4);
        let old = HashSet::from([portal]);
        let new = HashSet::new();
        let mut map = HashMap::from([(portal, 1)]);

        merge_portals_and_invalidate_if_needed(old, new, &mut map);
        assert_eq!(map.get(&portal), Some(&2));
    }

    #[test]
    fn merge_portals_and_invalidate_if_needed_remove_portal_on_threshold_exceeded() {
        let old_portal = hashed(3, 3, 6, 6);
        let new_portal = hashed(5, 5, 5, 5);
        let old = HashSet::from([old_portal]);
        let new = HashSet::from([new_portal]);
        let mut map = HashMap::from([(old_portal, 2)]); // Already at threshold

        let result = merge_portals_and_invalidate_if_needed(old, new, &mut map);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], new_portal.inner);
        assert!(!map.contains_key(&old_portal));
    }

    #[test]
    fn merge_portals_and_invalidate_if_needed_clear_on_overflow() {
        let mut old = HashSet::new();
        let mut new = HashSet::new();
        let mut map = HashMap::new();

        for i in 0..MAX_PORTALS_COUNT + 1 {
            let portal = hashed(i as i32, i as i32, 1, 1);
            old.insert(portal);
            new.insert(portal);
            map.insert(portal, 0);
        }

        let result = merge_portals_and_invalidate_if_needed(old, new, &mut map);
        assert_eq!(result.len(), 0);
        assert!(map.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn update_threshold_detection_success_resets_fail_count() {
        let mut threshold = Threshold::new(2);
        threshold.value = Some(Point::new(1, 2));
        threshold.fail_count = 1;
        let mut task = None;
        let mut detector = MockDetector::new();
        detector.expect_clone().returning(MockDetector::default);
        let context = Context::new(None, Some(detector));

        while task
            .as_ref()
            .is_none_or(|task: &Task<Result<Point>>| !task.completed())
        {
            threshold =
                update_threshold_detection(&context, 0, threshold, &mut task, |_detector| {
                    Ok(Point::new(5, 5))
                });
            time::advance(Duration::from_millis(1000)).await;
        }

        assert_eq!(threshold.value, Some(Point::new(5, 5)));
        assert_eq!(threshold.fail_count, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn update_threshold_detection_fail_increments_until_limit() {
        let mut threshold = Threshold::new(2);
        threshold.value = Some(Point::new(3, 3));
        threshold.fail_count = 0;

        let mut task = None;
        let mut detector = MockDetector::new();
        detector.expect_clone().returning(MockDetector::default);
        let context = Context::new(None, Some(detector));

        while task
            .as_ref()
            .is_none_or(|task: &Task<Result<Point>>| !task.completed())
        {
            threshold =
                update_threshold_detection(&context, 0, threshold, &mut task, |_detector| {
                    Err(anyhow!("fail"))
                });
            time::advance(Duration::from_millis(1000)).await;
        }

        assert_eq!(threshold.value, Some(Point::new(3, 3)));
        assert_eq!(threshold.fail_count, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn update_threshold_detection_fail_resets_value_after_limit() {
        let mut threshold = Threshold::new(2);
        threshold.value = Some(Point::new(3, 3));
        threshold.fail_count = 1;

        let mut task = None;
        let mut detector = MockDetector::new();
        detector.expect_clone().returning(MockDetector::default);
        let context = Context::new(None, Some(detector));

        while task
            .as_ref()
            .is_none_or(|task: &Task<Result<Point>>| !task.completed())
        {
            threshold =
                update_threshold_detection(&context, 0, threshold, &mut task, |_detector| {
                    Err(anyhow!("fail again"))
                });
            time::advance(Duration::from_millis(1000)).await;
        }

        assert_eq!(threshold.value, None);
        assert_eq!(threshold.fail_count, 0);
    }
}
