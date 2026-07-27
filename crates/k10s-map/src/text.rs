use std::collections::{HashMap, VecDeque};

use gpui::{Font, Hsla, ShapedLine, SharedString, TextRun, WindowTextSystem, px};

pub(crate) const HUD_LINE_COUNT: usize = 6;
const LABEL_CACHE_CAPACITY: usize = 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CacheStats {
    pub(crate) hits: u64,
    pub(crate) misses: u64,
    pub(crate) evictions: u64,
}

impl CacheStats {
    pub(crate) fn since(self, earlier: CacheStats) -> CacheStats {
        CacheStats {
            hits: self.hits - earlier.hits,
            misses: self.misses - earlier.misses,
            evictions: self.evictions - earlier.evictions,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    text: SharedString,
    font: Font,
    size_bits: u32,
    color: Hsla,
}

struct BoundedCache {
    capacity: usize,
    entries: HashMap<CacheKey, ShapedLine>,
    insertion_order: VecDeque<CacheKey>,
    stats: CacheStats,
}

impl BoundedCache {
    fn new(capacity: usize) -> Self {
        assert!(capacity > 0);
        BoundedCache {
            capacity,
            entries: HashMap::with_capacity(capacity),
            insertion_order: VecDeque::with_capacity(capacity),
            stats: CacheStats::default(),
        }
    }

    fn shape(
        &mut self,
        text: SharedString,
        font: &Font,
        size_px: f32,
        color: Hsla,
        text_system: &WindowTextSystem,
    ) -> ShapedLine {
        let key = CacheKey {
            text: text.clone(),
            font: font.clone(),
            size_bits: size_px.to_bits(),
            color,
        };
        if let Some(line) = self.entries.get(&key) {
            self.stats.hits += 1;
            return line.clone();
        }

        self.stats.misses += 1;
        if self.entries.len() == self.capacity {
            let evicted = self
                .insertion_order
                .pop_front()
                .expect("a full text cache has an insertion-order entry");
            let removed = self.entries.remove(&evicted);
            debug_assert!(removed.is_some());
            self.stats.evictions += 1;
        }

        let run = TextRun {
            len: text.len(),
            font: font.clone(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let line = text_system.shape_line(text, px(size_px), &[run], None);
        self.insertion_order.push_back(key.clone());
        self.entries.insert(key, line.clone());
        line
    }

    fn shape_uncached(
        &mut self,
        text: SharedString,
        font: &Font,
        size_px: f32,
        color: Hsla,
        text_system: &WindowTextSystem,
    ) -> ShapedLine {
        self.stats.misses += 1;
        let run = TextRun {
            len: text.len(),
            font: font.clone(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        text_system.shape_line(text, px(size_px), &[run], None)
    }
}

pub(crate) struct TextCache {
    labels: BoundedCache,
    hud_lines: [String; HUD_LINE_COUNT],
    enabled: bool,
}

impl Default for TextCache {
    fn default() -> Self {
        TextCache {
            labels: BoundedCache::new(LABEL_CACHE_CAPACITY),
            hud_lines: std::array::from_fn(|_| String::with_capacity(128)),
            enabled: true,
        }
    }
}

impl TextCache {
    pub(crate) fn shape_label(
        &mut self,
        text: SharedString,
        font: &Font,
        size_px: f32,
        color: Hsla,
        text_system: &WindowTextSystem,
    ) -> ShapedLine {
        if self.enabled {
            self.labels.shape(text, font, size_px, color, text_system)
        } else {
            self.labels
                .shape_uncached(text, font, size_px, color, text_system)
        }
    }

    pub(crate) fn stats(&self) -> CacheStats {
        self.labels.stats
    }

    pub(crate) fn hud_lines_mut(&mut self) -> &mut [String; HUD_LINE_COUNT] {
        &mut self.hud_lines
    }

    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }
}

pub(crate) fn content_hash(text: &str) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    text.as_bytes().iter().fold(OFFSET, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(PRIME)
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui::{NoopTextSystem, TextSystem};

    use super::*;

    fn text_system() -> WindowTextSystem {
        WindowTextSystem::new(Arc::new(TextSystem::new(Arc::new(NoopTextSystem::new()))))
    }

    #[test]
    fn cache_is_bounded_and_fifo_deterministic() {
        let system = text_system();
        let font = gpui::font("Noto Sans");
        let color = gpui::rgb(0xffffff).into();
        let mut cache = BoundedCache::new(2);

        cache.shape("a".into(), &font, 12.0, color, &system);
        cache.shape("b".into(), &font, 12.0, color, &system);
        cache.shape("a".into(), &font, 12.0, color, &system);
        assert_eq!(
            cache.stats,
            CacheStats {
                hits: 1,
                misses: 2,
                evictions: 0
            }
        );

        cache.shape("c".into(), &font, 12.0, color, &system);
        assert_eq!(cache.entries.len(), 2);
        assert!(!cache.entries.keys().any(|key| key.text == "a"));
        assert_eq!(cache.stats.evictions, 1);
    }

    #[test]
    fn content_hash_is_stable_and_content_sensitive() {
        assert_eq!(content_hash("pod-1"), content_hash("pod-1"));
        assert_ne!(content_hash("pod-1"), content_hash("pod-2"));
    }
}
