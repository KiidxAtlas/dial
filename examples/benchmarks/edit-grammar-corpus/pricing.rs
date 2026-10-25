//! Pricing helpers used by the checkout flow.
use std::collections::BTreeMap;

pub struct Line {
    pub sku: String,
    pub quantity: u32,
    pub unit_cents: u64,
}

pub fn line_total(line: &Line) -> u64 {
    let quantity = u64::from(line.quantity);
    let total = quantity * line.unit_cents;
    total
}

pub fn discount_cents(total: u64, percent: u64) -> u64 {
    if percent == 0 {
        return 0;
    }
    if percent > 100 {
        return total;
    }
    total * percent / 100
}

pub fn order_total(lines: &[Line], percent: u64) -> u64 {
    let mut sum = 0;
    for line in lines {
        sum += line_total(line);
    }
    let discount = discount_cents(sum, percent);
    sum - discount
}

pub fn totals_by_sku(lines: &[Line]) -> BTreeMap<String, u64> {
    let mut totals = BTreeMap::new();
    for line in lines {
        let entry = totals.entry(line.sku.clone()).or_insert(0);
        *entry += line_total(line);
    }
    totals
}

pub fn largest_line(lines: &[Line]) -> Option<u64> {
    let mut best = None;
    for line in lines {
        let total = line_total(line);
        if best.map_or(true, |current| total > current) {
            best = Some(total);
        }
    }
    best
}
