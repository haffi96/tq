//! Rolling average/max windows, reset each time the UI reads them (the same
//! `avg/max over the last report` the web overlay shows).

#[derive(Debug, Clone, Copy, Default)]
pub struct Window {
    pub avg: Option<f64>,
    pub max: Option<f64>,
}

impl std::fmt::Display for Window {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.avg, self.max) {
            (Some(a), Some(m)) => write!(f, "{a:.1}/{m:.1}"),
            _ => f.write_str("–"),
        }
    }
}

#[derive(Debug, Default)]
pub struct Win {
    sum: f64,
    n: u32,
    max: f64,
}

impl Win {
    pub fn add(&mut self, v: f64) {
        if self.n == 0 || v > self.max {
            self.max = v;
        }
        self.sum += v;
        self.n += 1;
    }

    pub fn take(&mut self) -> Window {
        let out = if self.n == 0 {
            Window::default()
        } else {
            Window {
                avg: Some(self.sum / self.n as f64),
                max: Some(self.max),
            }
        };
        *self = Self::default();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_takes_and_resets() {
        let mut w = Win::default();
        assert!(w.take().avg.is_none());
        w.add(-2.0);
        w.add(4.0);
        let t = w.take();
        assert_eq!((t.avg, t.max), (Some(1.0), Some(4.0)));
        assert!(w.take().avg.is_none());
    }
}
