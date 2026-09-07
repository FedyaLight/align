//! align-core: portable sync engine. No Apple frameworks.
//!
//! Port map (Swift -> Rust):
//! - Model.swift -> `model`
//! - Fingerprint.swift (Accelerate) -> `fingerprint` (realfft)
//! - GCCPHAT.swift (Accelerate) -> `gccphat` (rustfft)
//! - FingerprintCache.swift (CryptoKit+plist) -> `cache` (BLAKE3+bincode)
//! - FingerprintMatcher.swift -> `matcher` (identical thresholds)
//! - MatchGraph.swift -> `graph` (identical IRLS solve)
//! - PiecewiseTimeMapping.swift -> `piecewise`
//! - TimelineTrackAllocator.swift -> `allocator`
//! - FineMatcher / drift policy -> `fine` / `drift`
//! - Timeline import/export -> `xml` / `export`

pub mod allocator;
pub mod cache;
pub mod drift;
pub mod export;
pub mod fine;
pub mod fingerprint;
pub mod gccphat;
pub mod graph;
pub mod matcher;
pub mod meta;
pub mod model;
pub mod order;
pub mod parallel;
pub mod piecewise;
pub mod redirect;
pub mod spanned;
pub mod timecode;
pub mod timing;
pub mod wav;
pub mod xml;

pub use allocator::{TimelineTrackRequest, allocate, source_key_for_clip, source_key_for_url};
pub use cache::{
    CACHE_MAX_AGE_DAYS, CACHE_VERSION, CacheSettings, CacheStatistics, FingerprintCache,
};
pub use export::model as export_model;
pub use fine::{
    AudioWindow, FINE_SAMPLE_RATE, RefineEvent, RefineStage, WindowProvider, refine_forest,
};
pub use fingerprint::{
    FRAME_SIZE, Fingerprint, FingerprintExtractor, HOP_SIZE, SAMPLE_RATE, SearchAccuracy,
};
pub use gccphat::{GccPhatResult, align as gcc_phat_align};
pub use graph::{SolvedIsland, SolvedPlacement, solve as solve_graph};
pub use matcher::{
    ClipFingerprints, ClipTimingHints, MatchPolicy, MatchThreshold, PairAlignmentPoint,
    PairwiseMatch, match_fingerprints, policy,
};
pub use model::*;
pub use order::{
    ClipOrder, ClipOrderContext, ClipOrderPolicy, TrackContent, TrackContentPolicy,
    enforce_clip_order, enforce_track_content,
};
pub use parallel::par_map;
pub use piecewise::{MapPoint, PiecewiseTimeMapping, SolvedMappingPoint};
pub use spanned::{SpannedAnalysis, analyze_spans};
pub use timecode::analyze_timecodes;
pub use timing::{RangeAccumulator, VideoTimingInspection, canonical_frame_duration, classify};
pub use xml::{
    DraftEdit, DraftMediaKind, DraftTransition, ImportError, TimelineDraft, XmlDoc, read_timeline,
    timeline_sequence_summaries,
};
