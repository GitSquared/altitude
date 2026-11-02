// What can take the focus is what the screen drew. A ring kept beside the drawing
// is a second copy of the layout, and the copy is the one that goes stale: a pane
// behind a tab stays in the ring and eats the keypress meant for the next pane.
pub struct FocusRing<T> {
    targets: Vec<T>,
}

impl<T> Default for FocusRing<T> {
    fn default() -> Self {
        Self {
            targets: Vec::new(),
        }
    }
}

impl<T: Copy + PartialEq> FocusRing<T> {
    pub fn clear(&mut self) {
        self.targets.clear();
    }

    // Registered in the order the panes are read, so the ring follows the screen
    // rather than the order the drawing code happens to run in.
    pub fn add(&mut self, target: T) {
        if !self.contains(target) {
            self.targets.push(target);
        }
    }

    pub fn contains(&self, target: T) -> bool {
        self.targets.contains(&target)
    }

    pub fn next(&self, from: T, forward: bool) -> T {
        let Some(at) = self.targets.iter().position(|target| *target == from) else {
            // The focused pane is not on screen any more, which is where the first
            // thing that is belongs.
            return self.first(from);
        };
        let step = if forward { 1 } else { self.targets.len() - 1 };
        self.targets[(at + step) % self.targets.len()]
    }

    fn first(&self, fallback: T) -> T {
        self.targets.first().copied().unwrap_or(fallback)
    }
}
