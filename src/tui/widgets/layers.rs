#[derive(Default)]
pub struct Behind<T>(Option<T>);

impl<T> Behind<T> {
    pub fn cover(&mut self, under: T) {
        self.0 = Some(under);
    }

    pub fn uncover(&mut self) -> Option<T> {
        self.0.take()
    }

    pub fn under(&self) -> Option<&T> {
        self.0.as_ref()
    }
}
