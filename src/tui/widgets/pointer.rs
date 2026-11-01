use ratatui::layout::Rect;

pub struct Targets<T> {
    entries: Vec<(Rect, T)>,
}

impl<T> Default for Targets<T> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}

impl<T> Targets<T> {
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn add(&mut self, area: Rect, target: T) {
        self.entries.push((area, target));
    }

    pub fn at(&self, column: u16, row: u16) -> Option<&T> {
        self.entries
            .iter()
            .rev()
            .find(|(area, _)| {
                column >= area.x && column < area.right() && row >= area.y && row < area.bottom()
            })
            .map(|(_, target)| target)
    }
}
