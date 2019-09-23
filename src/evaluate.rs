//! Check if a reduction makes file smaller, and keep best reductions.
//! Works asynchronously when possible

use crate::atomicmin::AtomicMin;
use crate::deflate;
use crate::png::PngData;
use crate::png::PngImage;
use crate::png::STD_COMPRESSION;
use crate::png::STD_FILTERS;
use crate::png::STD_STRATEGY;
use crate::png::STD_WINDOW;
#[cfg(not(feature = "parallel"))]
use crate::rayon;
use crate::Deadline;
#[cfg(feature = "parallel")]
use rayon;
use rayon::prelude::*;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::SeqCst;
#[cfg(feature = "parallel")]
use std::sync::mpsc::*;
use std::sync::Arc;
use std::thread;

struct Candidate {
    image: PngData,
    // compressed size multiplier. Fudge factor to prefer more promising formats.
    bias: f32,
    // if false, that's baseline file to throw away
    is_reduction: bool,
    filter: u8,
    // first wins tie-breaker
    nth: usize,
}

#[derive(Default)]
struct Comparator {
    best_result: Option<Candidate>,
}

impl Comparator {
    fn evaluate(&mut self, new: Candidate) {
        // a tie-breaker is required to make evaluation deterministic
        let is_best = if let Some(ref old) = self.best_result {
            // ordering is important - later file gets to use bias over earlier, but not the other way
            // (this way bias=0 replaces, but doesn't forbid later optimizations)
            let new_len = (new.image.idat_data.len() as f64
                * if new.nth > old.nth {
                    f64::from(new.bias)
                } else {
                    1.0
                }) as usize;
            let old_len = (old.image.idat_data.len() as f64
                * if new.nth < old.nth {
                    f64::from(old.bias)
                } else {
                    1.0
                }) as usize;
            // choose smallest compressed, or if compresses the same, smallest uncompressed, or cheaper filter
            let new = (
                new_len,
                new.image.raw.data.len(),
                new.image.raw.ihdr.bit_depth,
                new.filter,
                new.nth,
            );
            let old = (
                old_len,
                old.image.raw.data.len(),
                old.image.raw.ihdr.bit_depth,
                old.filter,
                old.nth,
            );
            // <= instead of < is important, because best_candidate_size has been set already,
            // so the current result may be comparing its size with itself
            new <= old
        } else {
            true
        };
        if is_best {
            self.best_result = if new.is_reduction { Some(new) } else { None };
        }
    }

    fn get_result(self) -> Option<PngData> {
        self.best_result.map(|res| res.image)
    }
}

/// Collect image versions and pick one that compresses best
pub(crate) struct Evaluator {
    deadline: Arc<Deadline>,
    nth: AtomicUsize,
    best_candidate_size: Arc<AtomicMin>,
    eval_comparator: std::cell::RefCell<Comparator>,
}

impl Evaluator {
    pub fn new(deadline: Arc<Deadline>) -> Self {
        Self {
            deadline,
            best_candidate_size: Arc::new(AtomicMin::new(None)),
            nth: AtomicUsize::new(0),
            eval_comparator: Default::default(),
        }
    }

    /// Set baseline image. It will be used only to measure minimum compression level required
    pub fn set_baseline(&self, image: Arc<PngImage>) {
        self.try_image_inner(image, 1.0, false)
    }

    /// Check if the image is smaller than others
    /// Bias is a value in 0..=1 range. Compressed size is multiplied by
    /// this fraction when comparing to the best, so 0.95 allows 5% larger size.
    pub fn try_image(&self, image: Arc<PngImage>, bias: f32) {
        self.try_image_inner(image, bias, true)
    }

    fn try_image_inner(&self, image: Arc<PngImage>, bias: f32, is_reduction: bool) {
        let nth = self.nth.fetch_add(1, SeqCst);
        // These clones are only cheap refcounts
        let deadline = self.deadline.clone();
        let best_candidate_size = self.best_candidate_size.clone();
        // sends it off asynchronously for compression,
        // but results will be collected via the message queue
        // rayon::spawn(move || {
        let filters_iter = STD_FILTERS.par_iter().with_max_len(1);

        // Updating of best result inside the parallel loop would require locks,
        // which are dangerous to do in side Rayon's loop.
        // Instead, only update (atomic) best size in real time,
        // and the best result later without need for locks.
        filters_iter.for_each(|&filter| {
            if deadline.passed() {
                return;
            }
            if let Ok(idat_data) = deflate::deflate(
                &image.filter_image(filter),
                STD_COMPRESSION,
                STD_STRATEGY,
                STD_WINDOW,
                &best_candidate_size,
                &deadline,
            ) {
                best_candidate_size.set_min(idat_data.len());
                // the rest is shipped to the evavluation/collection thread
                let new = Candidate {
                    image: PngData {
                        idat_data,
                        raw: Arc::clone(&image),
                    },
                    bias,
                    filter,
                    is_reduction,
                    nth,
                };
            }
        });
        // });
    }
}
