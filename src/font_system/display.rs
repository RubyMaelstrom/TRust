//! CSS Fonts 4 #font-display-timeline / #font-display-desc (CSSWG
//! 81c27f686901): the first use starts one timer for a face, independently
//! of its resource URLs, source fallback, and other faces sharing a URL.

use super::FontDisplay;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    Block,
    Swap,
    Ready,
    Failed,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Timeline {
    started: Instant,
    display: FontDisplay,
    pub phase: Phase,
}

impl Timeline {
    #[cfg(test)]
    pub fn elapse(&mut self, duration: std::time::Duration) {
        self.started -= duration;
    }

    pub fn new(display: FontDisplay, now: Instant) -> Self {
        Self {
            started: now,
            display,
            phase: Phase::Block,
        }
    }

    pub fn update(
        &mut self,
        display: FontDisplay,
        loaded: bool,
        failed: bool,
        now: Instant,
    ) -> bool {
        self.display = display;
        let previous = self.phase;
        // Once optional/fallback has failed, a late download cannot change
        // the page's layout. A successful face has no further deadlines.
        if !matches!(self.phase, Phase::Ready | Phase::Failed) {
            let (block, swap) = display.periods();
            self.phase = if failed || swap.is_some_and(|swap| now >= self.started + block + swap) {
                Phase::Failed
            } else if loaded {
                Phase::Ready
            } else if now < self.started + block {
                Phase::Block
            } else {
                Phase::Swap
            };
        }
        previous != self.phase
    }

    pub fn next(&self) -> Option<Instant> {
        let (block, swap) = self.display.periods();
        match self.phase {
            Phase::Block => Some(self.started + block),
            Phase::Swap => swap.map(|swap| self.started + block + swap),
            Phase::Ready | Phase::Failed => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn font_display_timeline_keeps_first_use_across_sources_and_rejects_late_fonts() {
        let start = Instant::now();
        let mut font = Timeline::new(FontDisplay::Fallback, start);
        assert_eq!(font.next(), Some(start + Duration::from_millis(100)));
        assert!(font.update(
            FontDisplay::Fallback,
            false,
            false,
            start + Duration::from_millis(100)
        ));
        assert_eq!(font.phase, Phase::Swap);
        assert_eq!(font.next(), Some(start + Duration::from_millis(3100)));
        // Trying another src cannot restart either period.
        assert!(!font.update(
            FontDisplay::Fallback,
            false,
            false,
            start + Duration::from_secs(2)
        ));
        assert!(font.update(
            FontDisplay::Fallback,
            true,
            false,
            start + Duration::from_secs(4)
        ));
        assert_eq!(font.phase, Phase::Failed);
        assert!(!font.update(
            FontDisplay::Block,
            true,
            false,
            start + Duration::from_secs(5)
        ));
        assert!(font.next().is_none());
    }

    #[test]
    fn font_display_early_success_and_network_failure_end_the_timer() {
        let start = Instant::now();
        for display in [
            FontDisplay::Auto,
            FontDisplay::Block,
            FontDisplay::Swap,
            FontDisplay::Fallback,
            FontDisplay::Optional,
        ] {
            let mut loaded = Timeline::new(display, start);
            assert!(loaded.update(display, true, false, start + Duration::from_millis(1)));
            assert_eq!(loaded.phase, Phase::Ready);
            assert!(!loaded.update(display, true, false, start + Duration::from_secs(10)));
            assert!(loaded.next().is_none());
            let mut failed = Timeline::new(display, start);
            assert!(failed.update(display, false, true, start + Duration::from_millis(1)));
            assert_eq!(failed.phase, Phase::Failed);
            assert!(failed.next().is_none());
        }
    }
}
