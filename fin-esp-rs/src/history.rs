use crate::config::Screen;

const LEN: usize = 60;

/// Circular price history buffer — one slot per asset, 60 samples each.
/// At 45 s fetch intervals that covers ~45 minutes of history.
pub struct PriceHistory {
    prices: [[f64; LEN]; Screen::COUNT],
    heads:  [usize; Screen::COUNT],
    counts: [usize; Screen::COUNT],
}

impl PriceHistory {
    pub fn new() -> Self {
        Self {
            prices: [[0.0; LEN]; Screen::COUNT],
            heads:  [0; Screen::COUNT],
            counts: [0; Screen::COUNT],
        }
    }

    pub fn push(&mut self, screen: Screen, price: f64) {
        if price <= 0.0 { return; }
        let i = screen as usize;
        self.prices[i][self.heads[i]] = price;
        self.heads[i] = (self.heads[i] + 1) % LEN;
        if self.counts[i] < LEN { self.counts[i] += 1; }
    }

    /// Fill `out` with the most recent samples in chronological order (oldest first).
    /// Returns the number of samples written (≤ out.len()).
    pub fn get(&self, screen: Screen, out: &mut [f64]) -> usize {
        let i = screen as usize;
        let count = self.counts[i].min(out.len());
        if count == 0 { return 0; }
        let start = (self.heads[i] + LEN - count) % LEN;
        for j in 0..count {
            out[j] = self.prices[i][(start + j) % LEN];
        }
        count
    }
}
